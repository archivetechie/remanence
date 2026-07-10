# Tape I/O pipelined submission (TIO-5) — staging ring, hot submitter, deferred accounting — Design v0.1

**Status:** DRAFT v0.1 — panel review pending (2026-07-10).
**Problem source:** 2026-07-07b physical MSL3040 window. Post-TIO-1..4 pure
transfer sustained 157–164 MiB/s at 1 MiB/command (sg reserved-buffer grant),
measured cadence 6.2–7.2 ms/command. Morning dd battery through the same HPE
E208e + drive: kernel st path sustained 246 MB/s @ bs=1M and 293 MB/s @
bs=256K — ≈ HH LTO-9 native rate. Wire+kernel cost of a 1 MiB command is
therefore ~0.85–1 ms; the remaining ~5 ms/command is rem host-side work
serialized between consecutive SG_IO ioctls. This design removes that gap.
**Sequencing decision (the owner, 2026-07-10):** clean-slate design FIRST,
independent of the st driver; a bounded st-harvest pass runs AFTER freeze as
review findings only (never structural input). This revises the st-parity
brief's "audit BEFORE/WITH TIO-5" sequencing; the brief's other legs
(conformance oracle, lore watcher, qualification policy) are unaffected.
**Related:** `design-tape-io-throughput-v0.1.md` (frozen v0.2 — TIO-1..4,
whose error machinery and commit ordering this design must not alter),
`memo-field-window-2026-07-07b.md` (field evidence),
`design-st-parity-program.md` (pass-2 harvest source),
`layer5-multi-object-append-design-v0.1.md` (durable-boundary rules),
`lto9-media-readiness-design-v0.1.md` (fence/dirty-scope rules).

---

## 1. The gap, decomposed

Per command today (`write_block_batch`,
`crates/remanence-library/src/handle/tape_io/mod.rs:735`): ensure-position →
validate → seed position → build CDB → `fire_tape_started` audit emission
(mutex on shared drive state) → `set_timeout_for` → blocking SG_IO
(`execute_out`; sg indirect I/O memcpys the payload into the reserved buffer
inside the ioctl) → advance arithmetic cursor → `finish_tape_success` audit
emission (mutex again). Upstream, each batch arrives as a freshly allocated
`Vec<u8>` (`StagedSinkCommand::WriteBatch`,
`crates/remanence-api/src/pool_write.rs:1220`) filled by the producer thread
(one copy from spool-read block buffers into the batch Vec; a second copy
inside the sg driver). No single item is large; their sum — allocation,
copies, two mutex-guarded audit events, channel/thread handoffs, per-command
timeout setup — is a fixed ~5 ms tax paid serially between ioctls. This
matches the field observation that throughput scaled with batch size until
the sg grant clamped it: the tax is per-command, not per-byte.

## 2. Approaches considered

**A — Hot submitter loop, strictly one command in flight, everything else
prep-ahead (ADOPTED).** Feed-rate arithmetic: ioctl ≈ 0.85–1 ms for 1 MiB;
gap ≤ 0.3 ms ⇒ cadence ~1.3 ms ⇒ feed ~740 MB/s ⇒ drive-limited (~300 MB/s
HH LTO-9) with 2.5× headroom. No command overlap needed, ever.

**B — Async sg (`write()`/`poll()`/`read()`), queue depth 2 (REJECTED).**
A queued command cannot be reliably cancelled after the in-flight one fails;
the mid-layer may dispatch it past an EW/EOM/CHECK CONDITION point, breaking
partial-batch-uncommittable at its foundation. Buys ≤1 ms/command over A
while A's feed already exceeds drive rate 2.5×.

**C — io_uring SCSI passthrough (REJECTED).** Same non-cancellable-in-flight
hazard as B, plus kernel-feature dependency; no benefit over a blocking
ioctl on a dedicated thread at queue depth 1.

**st-driver verification (2026-07-10, kernel source):** st itself is
approach A: at most one outstanding command ever (`st_do_scsi` refuses a
second async command while `last_SRpnt` is live), write-behind
(`write_behind_check` waits for the prior async write before the next), no
tagged/multiple queueing for data transfers. st's difference is *caller-side*
asynchrony: buffered `write(2)` returns success before the SCSI command
completes, and a failure surfaces on a later syscall via shared buffer state,
unattributed to the write that caused it — the exact property that
disqualifies st for rem's commit protocol. TIO-5 takes the same one-in-flight
pipelining while keeping synchronous, exactly-attributed completion at the
commit boundary. Design conclusion: st's speed mechanism and A are the same
mechanism; we inherit the shape by convergence, not the baggage.

## 3. Architecture

Three roles replace the current producer/consumer:

### 3.1 Stager
Evolves the TIO-3 producer thread. Reads the spool **directly into** a fixed
ring of `staging_ring_buffers` (default 4) pre-allocated batch-sized buffers
(page-aligned; sized `effective_batch_blocks × block_size` after the sg
clamp), eliminating both per-batch allocation and the intermediate copy.
Pre-builds the fixed-mode WRITE(6) CDB (depends only on record count),
validates length/multiple-of-block-size. Filled buffers go on a bounded
submit queue; drained buffers return to the free list. Steady state performs
zero allocation.

### 3.2 Submitter (the hot loop)
Runs on the drive-actor thread that already owns the `DriveHandle` — no new
ownership or locking semantics. GOOD-path loop, in full: pop staged buffer →
SG_IO ioctl → status check → advance arithmetic cursor → push completion
record to the accounting queue → return buffer to ring → repeat. Nothing
else. Specifics:

- `set_timeout_for(TapeIo)` hoisted to transfer start (set once per
  transfer, restored when leaving the data phase), not per command.
- The 1 GiB position-drift tripwire is issued inline by the submitter
  (same thread, exact ordering); cost amortized to noise.
- On any non-GOOD result: stop submitting, drain accounting (§4), then run
  the **existing, unmodified** TIO-1/2 decode — EW/EOM arbitration by READ
  POSITION delta, deferred sense (0x71/0x73) → completion-unknown, partial
  batch → uncommittable + durable tape-I/O fence. One command in flight ⇒
  attribution exact ⇒ semantics identical by construction.

### 3.3 Spool ceiling consequence
With the submitter feeding ~740 MB/s, spool read rate is again the binding
ceiling. The TIO-4 tmpfs guidance is **required** for the 300 MB/s target
(root-disk spool caps ≈223 MiB/s); the acceptance run must state spool
placement. Stager-reads-into-ring removes one copy from the spool path.

## 4. Accounting lane (audit/diag off the critical path)

An ordered queue of per-command completion records (op, CDB summary,
duration, positions, records) drained by a companion thread.

- **Coalesced bulk audit:** data-transfer commands emit one TapeWrite audit
  span per staging window (command count, bytes, duration histogram) instead
  of two events per command (600 events/s at target rate is evidence noise,
  not evidence). Exact per-command numbers persist in diag counters.
- **Individually audited, immediately:** errors, EW/EOM signals, tripwire
  RPs and mismatches, filemarks, fences, session open/close.
- **Drain barriers (mandatory):** the accounting queue is fully drained and
  the audit sink flushed (a) before the object-closing WRITE FILEMARKS is
  issued, and (b) before any error propagates upward — every commit decision
  and every fence sees a complete audit record. Cost: one drain per object.
- **Crash honesty:** audit is evidence, not commit authority (journal +
  SQLite are). A crash may lose in-queue spans for an object that never
  committed — equivalent to today's unflushed-sink exposure. Stated, not
  hidden.

## 5. Read path symmetry + read diag parity

Same ring, reversed: submitter pops an *empty* buffer, issues the fixed-mode
batch READ (SILI=0; existing never-cross-a-filemark clamp and
FILEMARK+VALID residual decode unchanged), hands the full buffer to the
consumer (gRPC relay / restore writer), which returns it to the ring. One
command in flight; next READ's CDB staged while the consumer drains.

**Read diag parity (closes field gap #4):** the read path gains the write
path's full decomposition — per-phase times (locate/position, transfer,
relay), per-command cadence histogram (`gap_us`, `ioctl_us`), batch
effectiveness, bytes/records — in restore diag lines. Claim discipline: this
design does NOT promise to fix the field's 13.28 MB/s read number; it
promises the number becomes decomposable (drive rate vs relay rate vs client
write rate) and that the submission-gap component is removed.

## 6. Invariants and crash table

Testable claims, all asserted hermetically:

1. **Exactly one SCSI data command in flight, ever** (model transport
   asserts no overlapping execute).
2. **CDB-stream equivalence:** pipelining ON yields a byte-identical CDB
   sequence to pipelining OFF for the same input — same WRITEs, same
   tripwire RPs, same filemarks; only timing changes.
3. **Commit barrier unchanged:** all data GOOD → accounting drained →
   blocking WRITE FILEMARKS → DevicePositionProof → journal fsync → SQLite.
   The layer-5 ordering and the PositionProof type discipline (computed
   positions cannot reach the journal) are untouched.
4. **Error machinery relocated, not rewritten:** same decode functions, same
   fence scopes (TIO-2 durable tape-I/O fence), same completion-unknown
   rules.

Crash table (extends TIO-3 §5.1; chaos kill per row):

| Kill point | Recovery rule |
|---|---|
| mid-ioctl (command in flight) | transport-unknown → dirty fence at reopen; identical to today (one in flight = today's exposure) |
| staged-but-unsubmitted ring buffers | process-local; committed prefix authoritative; spool deleted on restart |
| accounting queue non-empty | evidence-only loss for an uncommitted object (§4); journal/catalog unaffected; fence rules apply |
| after last data GOOD, before filemark | unchanged layer-5 row: tail without journal record is fenced, never adopted |
| tripwire mismatch before durable fence persists | unchanged TIO row: startup reconciliation re-detects; uncommitted tail fenced |

## 7. Config and backout

```toml
[tape_io]
pipelined_submission = true    # false = exact TIO-3/4 staged behavior
staging_ring_buffers = 4       # pre-allocated batch buffers per transfer
```

Backout ladder: `legacy_single_block` > `pipelined_submission` (legacy
implies pipelining off). Memory: ring × effective batch bytes per active
drive (4 MiB at the current 1 MiB grant; 16 MiB if the grant reaches 4 MiB) —
documented, trivial. `write_batch_blocks`/`read_batch_blocks` semantics
unchanged.

## 8. Instrumentation (the field-verifiable mechanism)

Per-command on the submitter: `gap_us` (previous completion → this
submission) and `ioctl_us`. Diag summary: p50/p95/max gap, cadence, effective
feed rate, alongside existing `effective_batch_blocks` and `position_calls`.

Physical acceptance (next MSL3040 window):
- p95 submit gap ≤ 500 µs; **sustained pure transfer ≥ 280 MB/s at the
  existing 1 MiB sg grant** (tmpfs spool, per §3.3);
- read run fully decomposable via the new diag;
- diag confirms mechanism, not just outcome (gap histogram, not throughput
  alone).

Consequences worth recording: pipelining demotes the sg-grant/HBA-knob chase
(and the optional Broadcom 9500-16e) from "next lever" to "optional
insurance — scale-out only." VTL benches will not show the physical win
(different wire economics); hermetic acceptance = invariants + counters, the
library proves the number.

## 9. Testing

Hermetic (model transport / chaos):
- single-command-in-flight assertion under pipelining;
- CDB-stream equivalence ON vs OFF (byte-identical command sequence);
- accounting drain barriers: filemark CDB never precedes a fully drained
  queue; error surfacing never precedes drain (inject slow accounting sink);
- non-GOOD mid-stream: submitter stops, no further data CDBs, existing
  decode outcomes reproduced exactly (EW residual, deferred sense, partial
  batch → fence);
- crash table rows (§6) via chaos kill injection;
- ring discipline: no allocation in steady state (buffer identity reuse
  asserted), bounded queue backpressure;
- read symmetry: boundary clamp + FILEMARK residual decode unchanged under
  pipelining; buffer-return discipline;
- config: `pipelined_submission=false` reproduces TIO-3/4 command stream;
  `legacy_single_block=true` still reproduces the original serial stream.
- Scenario: extend `scenario-append` `covers` with `rem.tape.pipelined_io`;
  full `~/system` suite green from clean slate.

## 10. Rollout

1. **TIO-5a** — write side: staging ring, submitter loop on the drive actor,
   accounting lane + drain barriers, timeout hoist, config keys, model-
   transport + chaos coverage.
2. **TIO-5b** — read side: read pipeline, read diag parity, runbook/fieldtest
   updates (tmpfs requirement note, acceptance additions), remaining
   hermetic coverage.
3. **st harvest pass (post-freeze):** bounded checklist against the frozen
   design — timeout ladder constants per command class, unit-attention retry
   taxonomy, EW-handling nuances from `st.c` — findings fed back as review
   deltas (semantics only, no code transcription; GPL provenance discipline
   per the st-parity brief).
4. Physical validation next window per §8; A7 (streaming ingest, kills the
   spool) and A1 (lazy dismount, kills the ~65 s close) remain the named
   next levers for end-to-end wall time.
