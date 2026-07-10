# Tape I/O pipelined submission (TIO-5) — staging ring, hot submitter, in-place accounting — Design v0.2

**Status:** v0.2 — panel folded 2026-07-10, verify round pending.
**Panel 2026-07-10:** 4 blind lenses (SCSI/SSC = Opus, concurrency = Opus,
cost/efficiency = Opus, failure-modes = codex **gpt-5.6-sol**): 1 blocker,
14 unique majors, 11 minors, 3 nits; all accepted, none rejected;
dispositions in `panel-tape-io-pipelined-submission-2026-07-10.md`.
Headline fold: the v0.1 deferred-accounting thread is **deleted** (its
premise — expensive per-command audit — was false: the audit hook is unwired
in production; grep evidence in the panel report), which dissolves the
blocker and six majors. v0.2 is simpler than v0.1.
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
validate → seed position → build CDB → audit fire (a `None` hook in
production — nanoseconds; the mutex is uncontended) → `set_timeout_for` (a
field write — nanoseconds) → blocking SG_IO (`execute_out`; sg indirect I/O
memcpys the payload into the reserved buffer inside the ioctl) → advance
arithmetic cursor → audit fire. Upstream, each batch arrives as a freshly
allocated `Vec<u8>` (`StagedSinkCommand::WriteBatch`,
`crates/remanence-api/src/pool_write.rs:1220`) filled by the producer thread
(one copy from spool-read block buffers into the batch Vec; a second copy
inside the sg driver). The ~5 ms/command tax is dominated by allocation,
copies, and channel/thread handoffs — per-command, not per-byte, which
matches the field observation that throughput scaled with batch size until
the sg grant clamped it. (Panel correction: audit emission and timeout setup
are NOT meaningful contributors; v0.1 overweighted them.) The exact
decomposition is confirmed empirically by §8's cadence instrumentation.

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

Two moving parts replace the current producer/consumer (v0.1's third thread,
the accounting drainer, was deleted by the panel fold — see §4):

### 3.1 Stager and staging ring
Evolves the TIO-3 producer thread. Reads the spool **directly into** a fixed
ring of `staging_ring_buffers` (default 4, validated range 2..=16, checked
allocation, effective per-drive ring bytes logged at open) pre-allocated
batch-sized buffers (sized `effective_batch_blocks × block_size` after the
sg clamp), eliminating both per-batch allocation and the intermediate copy.
Buffer alignment: page-aligned for the *spool-read* path (enables O_DIRECT
later); the sg side needs no alignment — `execute_out` uses indirect I/O,
the kernel copies from our buffer regardless. Pre-builds the fixed-mode
WRITE(6) CDB (depends only on record count), validates
length/multiple-of-block-size. Filled buffers go on a bounded submit queue;
drained buffers return via a path with **capacity ≥ `staging_ring_buffers`**
so buffer return is always non-blocking (no two-bounded-channel credit-loop
wedge). Steady state performs zero allocation.

Ring/thread invariants (panel-folded, all asserted hermetically):
- **Only the submitter issues ioctls on the drive fd** (the per-fd sg
  reserved buffer is load-bearing; stager never touches the fd).
- **Position seeding stays in the submitter at issue time** — never
  prep-ahead into the stager. The EW/EOM READ POSITION delta arbitration
  requires `position_before` pinned to the exact pre-batch cursor; hoisting
  the seed would make it stale by ≥1 batch.
- **The trailing partial batch rebuilds its CDB** for its true record count;
  a reused full-size CDB would encode the wrong TRANSFER LENGTH.
- **Terminal poison protocol** (preserves TIO-3 semantics,
  `pool_write.rs:1568-1588`): on any error/poison, the submit channel is
  closed/disconnected **before any join**, queued buffers are drained and
  discarded without issuing CDBs, and buffer ownership returns to the ring —
  a stager parked in a blocking send is always released. Drop/join order:
  close channels → drain/discard → join stager.

### 3.2 Submitter (the hot loop)
Runs on the drive-actor thread that already owns the `DriveHandle` — no new
ownership or locking semantics. GOOD-path loop, in full: pop staged buffer →
`set_timeout_for(TapeIo)` (a field write; kept per-command — it is free and
self-heals the timeout class after any inline READ POSITION, which sets the
5 s TapeStatus class; the v0.1 hoist is DROPPED as both useless and unsafe) →
SG_IO ioctl → status check → **record the GOOD completion** (in-place
counters, §4) → advance arithmetic cursor → tripwire check → return buffer
to ring → repeat.

- **Cursor arithmetic is split from tripwire execution** (panel): the GOOD
  data command is recorded *before* the tripwire RP is issued; the tripwire
  is its own recorded operation. A GOOD WRITE can never lose its record to a
  subsequent RP failure.
- The 1 GiB position-drift tripwire is issued inline by the submitter (same
  thread, exact ordering); on mismatch: poison + durable fence, exactly the
  TIO-1/2 rule.
- The good path is a **new audit-free variant**; the TIO-1/2 decode
  *helpers* (`write_eom_signal`, `fixed_records_transferred_from_sense`,
  `records_delta_between`, deferred-sense classification) are extracted and
  reused **unmodified**. (v0.1's "reuse write_block_batch unmodified" was
  inaccurate — its audit fires are interleaved with the decode.)
- On any non-GOOD result: **stop → classify → fence → audit → propagate**,
  in that order. The submitter stops submitting; the raw result is
  immediately classified with the existing decode (EW/EOM arbitration by
  READ POSITION delta with position re-assertion of the TapeIo class after
  the inline RP; deferred sense 0x71/0x73 → completion-unknown; partial
  batch → uncommittable); position invalidation and durable-fence
  persistence run **independently of any audit emission**; audit records
  (which for errors carry raw status, sense bytes, transport fields, and
  decoded outcome exactly as today) emit after safety persistence; then the
  error propagates. One command in flight ⇒ attribution exact ⇒ decode
  semantics identical by construction.

### 3.3 Spool ceiling consequence
With the submitter feeding ~740 MB/s, spool read rate is again the binding
ceiling. The TIO-4 tmpfs guidance is **required** for the 300 MB/s target
(root-disk spool caps ≈223 MiB/s); the acceptance run must state spool
placement. Stager-reads-into-ring removes one copy from the spool path.

**Production RAM budget (panel):** append remains store-and-forward, so on
the DL385 dual-drive concurrent target, tmpfs spool needs ≈2× the largest
object footprint + ring memory (ring × batch bytes × drives) + OS headroom,
or the fail-closed tmpfs refusal (TIO-4 §6) caps concurrency. This is an
acceptance precondition, stated in §8. **A7 (streaming ingest) is the real
dual-drive-at-rate unlock** — it deletes the spool and this budget with it;
its priority rises accordingly (§10).

## 4. Accounting: synchronous, coalesced, in place

**Panel rewrite.** v0.1 proposed a companion accounting thread with drain
barriers; the panel found (a) its premise false — the per-command audit hook
is **unwired in production** (`SharedAuditAdapter::library_hook` has no
non-test callers; `fire_audit` hits a `None` hook — so today's per-command
audit cost is nanoseconds), and (b) its consequences severe (audit-sink
liveness gating commit; backpressure reintroducing the gap; racy drain
semantics). v0.2 therefore keeps accounting **on the submitter,
synchronous**:

- **In-place bulk accounting:** the submitter bumps preallocated counters
  and fixed histogram buckets (`gap_us`, `ioctl_us`, records, bytes) —
  array increments, no lock, no allocation, no queue.
- **Coalesced spans:** one TapeWrite audit span per staging window (command
  count, bytes, duration histogram summary) emitted synchronously — a
  handful of emissions per object. If the audit hook is ever wired to a slow
  sink, coalescing keeps it off the per-command path by construction.
- **Individually audited, inline, immediately** (exactly today's machinery
  and record content, sense bytes included): errors, EW/EOM, tripwire RPs
  and mismatches, filemarks, fences, session open/close.
- **Safety-persistence invariant (folded from the panel blocker):** fences,
  position invalidation, and poison **never depend on audit-sink
  availability**. Ordering on every path: classify → fence → audit →
  propagate. Audit failures are recorded out-of-band (existing
  `SharedAuditAdapter` behavior) and never block or reorder safety actions.
- **Per-window intent marker (forensic parity):** before entering the hot
  loop for a window, one diag/audit line names the planned CDB range. Today
  a Started event precedes every ioctl; under coalescing, a kill mid-ioctl
  would otherwise leave no trace of the in-flight command. The intent marker
  bounds that loss to "which window", and the regression is documented here
  rather than hidden.
- Completion/error records own their data by value — they never borrow the
  ring buffer, so buffer reuse cannot alias a record.

## 5. Read path symmetry + read diag parity

Same ring, reversed: submitter pops an *empty* buffer, issues the fixed-mode
batch READ (SILI=0; existing never-cross-a-filemark clamp unchanged), and
hands the consumer (gRPC relay / restore writer) a **typed handoff**:
`{buffer, valid_bytes, records_read, terminal flags (filemark/eod/error)}`.
The stale tail of a reused ring buffer past `valid_bytes` is never exposed
(sentinel-prefill test, §9); fail-closed outcomes (`read_core`'s current
behavior) withhold the buffer entirely. One command in flight; the next
READ's CDB may be staged while the consumer drains, **but any short/residual
outcome (FILEMARK+VALID residual, ILI, error) cancels the staged next CDB**;
the submitter recomputes the clamp (`remaining_records_in_file`, file
cursor) from post-decode state before issuing anything further — reads never
cross a tape-file boundary, including via a stale pre-staged CDB.

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
   tripwire RPs, same filemarks; only timing changes. **Preconditions
   (panel):** identical sg reserved size and tape-sourced block size at
   open; the trailing partial batch rebuilds its CDB. **Equivalence is
   proven beyond CDB bytes** (§9): timeout-class stream, audit event
   sequence, and poison behavior are compared too — the model transport
   gains timeout-class recording to make this visible (today its
   `set_timeout_for` is a no-op, which would let a timeout regression pass
   a CDB-only test).
3. **Commit barrier unchanged:** all data GOOD (recorded) → blocking WRITE
   FILEMARKS (which sets its own timeout class, unaffected by this design) →
   DevicePositionProof → journal fsync → SQLite. The layer-5 ordering and
   the PositionProof type discipline are untouched.
4. **Error machinery: decode helpers reused unmodified; ordering =
   classify → fence → audit → propagate** (§3.2, §4). Same fence scopes
   (TIO-2 durable tape-I/O fence), same completion-unknown rules.

Crash table (extends TIO-3 §5.1; chaos kill per row):

| Kill point | Recovery rule |
|---|---|
| mid-ioctl (command in flight) | transport-unknown → dirty fence at reopen; identical to today (one in flight = today's exposure). Intent marker (§4) bounds the forensic loss to the window |
| after data GOOD, before in-place record/cursor update | process-local state is never trusted on restart; journaled prefix authoritative; tape tail uncommitted → layer-5 fence rules |
| after cursor update, before tripwire RP completes | same rule; tripwire state is process-local; fence on reopen per readiness rules |
| mid-error-decode (raw non-GOOD held, fence not yet persisted) | startup reconciliation + journal/SQLite prefix comparison re-detects; object was never journaled → tail fenced. Conservative rule: lost decode ⇒ fence |
| after fence persisted, before error audit/propagation | fence is durable and authoritative; audit loss is evidence-only |
| staged-but-unsubmitted ring buffers | process-local; committed prefix authoritative |
| **spool file at SIGKILL** | **corrected (panel):** `Spool::Drop` removes only on normal unwinding — SIGKILL orphans the file (on tmpfs: RAM held across crashes, outside the budget). TIO-5b adds **startup orphan reconciliation**: enumerate owned `spool-*.bin`, record evidence, remove, account remaining tmpfs budget — before accepting writes |
| after last data GOOD, before filemark | unchanged layer-5 row: tail without journal record is fenced, never adopted |

## 7. Config and backout

```toml
[tape_io]
pipelined_submission = false   # SHIPS DEFAULT-OFF (panel): flip per-host
                               # after physical validation; false = exact
                               # TIO-3/4 staged behavior
staging_ring_buffers = 4       # validated 2..=16; checked allocation;
                               # effective per-drive ring bytes logged
```

Backout ladder: `legacy_single_block` > `pipelined_submission` (legacy
implies pipelining off). **Fleet rollout (panel):** `TapeIoConfig` rejects
unknown fields, so old binaries refuse the new keys — ship code fleet-wide
default-off first, drain active sessions, then enable canaries and widen.
Config is snapshotted at drive-open: editing the file affects neither active
sessions nor their backout — the **effective mode is exposed in live
status/diag** so an operator can see what a session is actually running.
Memory: ring × effective batch bytes per active drive (4 MiB at the current
1 MiB grant; 16 MiB if the grant reaches 4 MiB).

## 8. Instrumentation (the field-verifiable mechanism)

In-place histograms on the submitter (§4): `gap_us` (previous completion →
this submission) and `ioctl_us`, preallocated buckets, zero per-command
allocation. Diag summary: p50/p95/max gap, cadence, effective feed rate,
alongside existing `effective_batch_blocks` and `position_calls`.
Instrumentation is read from shared diag state, **never via a
`DriveCommand`** (the actor is busy for the whole transfer; a status query
through the actor mailbox would block — known property, unchanged from
TIO-3, now stated: operator abort cannot interrupt a mid-ioctl submitter).

Physical acceptance (next MSL3040 window), preconditions: tmpfs spool
(§3.3), RAM budget stated for the host, kit defect #9 pre-warm/resume-wait
fix applied for multi-drive legs:

1. **Single-drive write:** p95 submit gap ≤ 500 µs; sustained pure transfer
   ≥ 280 MB/s at the existing 1 MiB sg grant; gap histogram confirms the
   mechanism, not just the throughput.
2. **Dual-drive concurrent (restore + append) — the HBA-decision leg
   (panel):** aggregate ≈ 2× single-drive through the one E208e/smartpqi
   ring, per-drive cadence recorded. This is the config that actually
   retires (or revives) the 9500-16e purchase question on data.
3. **1 MiB vs 4 MiB command comparison:** raise `max_sectors_kb`/sg grant
   (zero-cost sysfs write) on one leg — decides whether the grant chase is
   permanently retired or kept as cheap margin.
4. **Read:** fully decomposable via the new diag; submission-gap component
   demonstrably gone.
5. **Receive-feed-rate counter** captured in the same window: the measured
   gRPC-receive → ring feed rate that the A7 streaming-ingest design starts
   from (free de-risk, panel).

Consequences worth recording: pipelining demotes the sg-grant/HBA-knob chase
to "optional margin" (leg 3 decides), and the 9500-16e to "optional
insurance" pending leg 2 — with one owner-level caveat: the E208e's two
external connectors physically cap parallel migration at 2 drives regardless
of throughput; >2-drive migration plans reopen the purchase on port count.
VTL benches will not show the physical win (different wire economics);
hermetic acceptance = invariants + counters, the library proves the number.

## 9. Testing

Hermetic (model transport / chaos):
- single-command-in-flight assertion under pipelining;
- CDB-stream equivalence ON vs OFF **plus** timeout-class stream, audit
  event sequence, and poison-behavior equivalence (model transport records
  timeout classes — new test infra, panel);
- timeout-class regression around the tripwire: WRITE after tripwire RP and
  after error-path RP runs at TapeIo (60 s), never TapeStatus (5 s) —
  success, mismatch, and RP-failure exits all covered;
- EW × tripwire interleave: `position_check_bytes` low enough that a
  tripwire RP and an EW event land in one window; `position_before` pin
  proven fresh;
- trailing partial batch: N not a multiple of B ⇒ rebuilt CDB with true
  record count;
- GOOD-record-before-tripwire: kill/fail injected between data GOOD, cursor
  update, RP submission, RP completion, fence persistence — the GOOD
  command's record survives in every row;
- non-GOOD mid-stream: submitter stops, no further data CDBs, decode
  outcomes reproduced exactly (EW residual, deferred sense, partial batch →
  fence); ordering classify → fence → audit → propagate asserted with a
  failing/slow audit sink (safety actions complete regardless);
- terminal poison protocol: stager parked in blocking send is released on
  every error path (close-before-join asserted); queued buffers drained
  without CDBs; no buffer leak/double-return (ring accounting balances);
- read: typed handoff `valid_bytes` honored — sentinel-prefilled ring buffer
  never leaks stale tail bytes; FILEMARK residual cancels the staged next
  CDB (assert absence of the boundary-crossing READ); clamp recomputed from
  post-decode state; fail-closed outcomes withhold the buffer;
- ring config: bounds validation (reject 0/1/>16), checked allocation,
  return-path capacity ≥ ring size (non-blocking return asserted);
- spool orphan reconciliation: SIGKILL leaves `spool-*.bin` → startup
  enumerates, evidences, removes, re-accounts tmpfs budget before accepting
  writes; foreign files untouched;
- crash table rows (§6) via chaos kill injection;
- config: `pipelined_submission=false` reproduces TIO-3/4 behavior
  (CDB + timeout + audit + poison equivalence, not CDB-only);
  `legacy_single_block=true` still reproduces the original serial stream;
  effective mode visible in status.
- Scenario: extend `scenario-append` `covers` with `rem.tape.pipelined_io`;
  full `~/system` suite green from clean slate.

## 10. Rollout

1. **TIO-5a** — write side: staging ring + invariants, submitter hot loop
   (audit-free good-path variant, decode helpers extracted unmodified),
   in-place accounting + coalesced spans + intent marker, poison protocol,
   config keys (default-off) + effective-mode status, model-transport
   timeout-class recording, hermetic coverage (§9 write-side rows).
2. **TIO-5b** — read side: read pipeline + typed handoff + staged-CDB
   cancellation, read diag parity, **spool orphan reconciliation**,
   runbook/fieldtest updates (tmpfs + RAM budget, acceptance legs 1–5,
   max_sectors sweep step), remaining hermetic coverage.
3. **st harvest pass (post-freeze):** bounded checklist against the frozen
   design — timeout ladder constants per command class, unit-attention retry
   taxonomy, EW-handling nuances from `st.c` — findings fed back as review
   deltas (semantics only, no code transcription; GPL provenance discipline
   per the st-parity brief).
4. Physical validation next window per §8 (flip `pipelined_submission` on
   validated hosts after acceptance); then **A7 streaming ingest** (now the
   named dual-drive-at-rate unlock — it deletes the spool and its RAM
   budget) and A1 lazy dismount take over the remaining end-to-end wall.
