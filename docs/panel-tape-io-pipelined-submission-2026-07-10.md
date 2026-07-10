# Panel report — TIO-5 pipelined submission — 2026-07-10

**Design:** `design-tape-io-pipelined-submission-v0.1.md` (v0.1 → v0.2 fold).
**Lenses (blind, parallel):** SCSI/SSC correctness = Opus; concurrency &
pipeline choreography = Opus; cost/efficiency = Opus (standing lens);
failure-modes/ops = **codex gpt-5.6-sol** (the owner-pinned).
**Raw verdicts:** SCSI 0b/2M/3m/1n · concurrency 0b/4M/4m/2n · cost 0b/3M/3m ·
codex 1b/11M/2m. After dedup: **1 blocker, 14 unique majors, 11 minors, 3 nits.**

## The pivotal fold (resolves the blocker + 6 majors at once)

**Cost-M1 (verified in code): the per-command audit hook is UNWIRED in
production** — `SharedAuditAdapter::library_hook` has no callers outside
tests (grep 2026-07-10); `fire_audit` hits a `None` hook. The v0.1 deferred
accounting lane (companion thread + queues + drain barriers) was therefore
built to move a nanosecond-scale cost off the hot path, while creating the
panel's worst findings:

- **codex BLOCKER:** "drain accounting then decode" made fence persistence /
  position invalidation wait on audit-sink liveness — violating the frozen
  error contract;
- concurrency M1 (audit given commit-liveness authority), M2 (backpressure
  reintroduces the gap), M3 ("drained" is racy without a handshake), codex
  M4/M5 (queue capacity/overflow, drain protocol), plus 3 minors.

**Disposition (ADOPTED): accounting is synchronous + coalesced, in place.**
The submitter bumps preallocated counters/histogram buckets (O(ns), no lock,
no alloc); one coalesced TapeWrite span per staging window is emitted
synchronously (a handful per object); errors/EW/EOM/tripwires/filemarks/
fences audit individually inline **exactly as today**. No companion thread,
no queues, no drain barriers. Explicit invariant added: **safety persistence
(fences, position invalidation, poison) never depends on audit-sink
availability.** All findings above dissolve by construction; the blocker's
ordering rule (classify → fence → audit → propagate) is folded as a stated
invariant anyway.

## Other accepted findings (all folded into v0.2)

| # | Source | Finding | Fold |
|---|---|---|---|
| 1 | SCSI-M1 = codex-M2 | Timeout hoist unsafe: tripwire/error-path RP sets 5 s TapeStatus (last-writer-wins transport state); next WRITE would run at 5 s → spurious transport-unknown → fence. And the hoist saved nothing (`set_timeout_for` is a field write, ns). | **Hoist DROPPED**; per-command `set_timeout_for` retained; removed from the §1 gap decomposition. |
| 2 | codex-M3 | Completion recorded only after cursor advancement, which embeds the tripwire RP and can fail — a GOOD WRITE could lose its record. | Cursor arithmetic split from tripwire execution; GOOD data command recorded first; tripwire is its own recorded op. Chaos kills added between the steps. |
| 3 | conc-M4 = codex-M6 | "Stop submitting" on error can deadlock a stager parked in a blocking send (TIO-3 deliberately drains-and-discards after poison to release producers). | Terminal poison protocol folded: close/disconnect submit channel before any join; drain-and-discard queued buffers without issuing CDBs; drop/join order stated. |
| 4 | SCSI-M2 | Short read (FILEMARK residual) invalidates the already-staged next-read CDB → would cross a file boundary. | Rule folded: any short/residual outcome cancels the staged CDB; clamp recomputed from post-decode state; hermetic test added. |
| 5 | codex-M8 | Handing the "full buffer" to the consumer on short reads exposes a stale ring tail. | Typed handoff folded: `{valid_bytes, records_read, terminal flags}`; stale tail never exposed; sentinel-prefill test added. |
| 6 | codex-M7 (+conc states) | Crash table missed new intermediate states (post-ioctl pre-cursor, mid-error-decode, RP-done-fence-pending…). | Crash table expanded to 8 rows; recovery rule: restart without a completed durable boundary ignores process-local state, journaled prefix authoritative, conservative fence on lost outcomes. |
| 7 | codex-M10 + SCSI-m2/m4 | "pipelined=false = exact shipped behavior" unproven by CDB-stream comparison alone; equivalence preconditions unstated; trailing partial batch CDB must be rebuilt. | §9 equivalence extended: timeout-class stream + audit event sequence + poison behavior compared (model transport gains timeout-class recording); preconditions pinned (same sg reserved size, tape-sourced block size); partial-batch CDB rebuild test. |
| 8 | codex-M11 | Rollout unsafe: default-true activates on upgrade restart; old binaries reject unknown config keys; config snapshot at drive-open invisible to editing. | **Ships default-OFF**; flip to on only after physical validation; ship-code-first fleet ordering + effective-mode in live status folded. |
| 9 | codex-M12 | "Spool deleted on restart" contradicts code: `Spool::Drop` only on normal unwind; SIGKILL orphans tmpfs spool files outside the budget. | Startup orphan reconciliation folded as a TIO-5b deliverable (enumerate owned `spool-*.bin`, evidence, remove, budget-account); §6 row corrected. |
| 10 | cost-M2 | HBA demotion proven single-drive only; dual-drive concurrent through one smartpqi ring untested; E208e port count (2 external) caps parallel migration regardless of MB/s. | Dual-drive concurrent leg added to §8 acceptance (with kit-defect-#9 pre-warm fix). Port-count point surfaced to owner (business item below). |
| 11 | cost-M3 | tmpfs spool now load-bearing → DL385 dual-drive concurrent RAM budget (~2× largest object + rings + OS) unstated; refusal caps concurrency. | Budget stated as acceptance precondition; **A7 reframed as the dual-drive-at-rate unlock** (removes spool RAM scaling entirely). |
| 12 | cost-m2/m3 | Free window additions: 1 MiB vs 4 MiB pipelined comparison (retire or keep the grant chase on data); receive-feed-rate counter (de-risks A7 design). | Both added to §8. |
| 13 | codex-m2 + conc-m2 | Ring bounds unvalidated (0/1 defeats pipeline; huge OOMs); free-list capacity vs bounded queues can credit-loop wedge. | `staging_ring_buffers` validated 2..=16, checked alloc, effective bytes logged; return path capacity ≥ ring size (always non-blocking). |
| 14 | SCSI-m3 | Prep-ahead framing invites hoisting position seeding into the stager → stale `position_before` breaks EW arbitration. | Invariant folded: position seeding stays in the submitter at issue time, never prep-ahead; EW×tripwire interleave test added. |
| 15 | codex-m1 | Forensic regression vs today: no Started event before each ioctl; kill mid-ioctl leaves no record of the in-flight command. | Per-window intent marker folded (one diag/audit line naming the planned CDB range before entering the loop) + regression documented. |
| 16 | conc-m3 | "Reuses TIO-1/2 decode unmodified" inaccurate — audit fires are interleaved inside `write_block_batch`. | Reworded: decode *helpers* reused unmodified (extracted); good path is a new audit-free variant. |
| 17 | SCSI-nit + conc-nits | Alignment rationale (sg indirect I/O needs none; it's for O_DIRECT spool reads); only the submitter touches the drive fd; completion records own their data (no ring-buffer borrows); instrumentation read outside the actor channel; operator abort cannot interrupt mid-ioctl (known non-goal, unchanged from TIO-3). | All folded as stated invariants/notes. |

**Rejected: none.** (SCSI-m5 was a confirmation, not a defect — the
position_before pin is correct as designed; its anti-regression invariant is
finding 14.)

## Business item for the owner (the one decision)

**9500-16e HBA posture.** The dd battery killed the *throughput* argument for
the purchase, and TIO-5 removes the *cadence* argument. But the E208e has
**two external connectors — a physical cap of 2 drives for parallel
migration, independent of speed**. If the central-IT migration plan ever
calls for >2 drives in parallel, the purchase returns as a port-count
question. **Recommendation:** buy nothing now; keep the 9500-16e diligence
row warm (STANDING/backlog note tied to "migration drive scale-out"), and
add the dual-drive acceptance leg (folded, §8) so the 2-drive config is
proven on data. Decide only if/when the migration plan needs >2 drives.

## Process notes

- Lens agreement was high-value: cost-M1 (unwired hook) + concurrency M1–M3 +
  codex blocker all triangulated the same component from three directions;
  the fold *deletes* that component rather than patching it — v0.2 is
  simpler than v0.1 (one fewer thread, no new queues, 2 fewer crash rows
  than the barrier design implied).
- codex (gpt-5.6-sol) produced the deepest code-grounded findings (spool
  orphan contradiction, config-compat rollout, completion-record shape) —
  good second-opinion yield; worth pinning again for the verify round.
