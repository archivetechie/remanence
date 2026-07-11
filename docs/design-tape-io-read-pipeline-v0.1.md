# Tape I/O read pipeline (TIO-6) — read submitter, read reservoir + anti-shoe-shine controller, relay unblocking — Design v0.2

**Status:** **FOLDED v0.2** (2026-07-11) — panel review folded + owner decisions
incorporated; pending one codex verify round before freeze.
**Fold record:** panel 2026-07-11: 37 findings (14 failure / 9 concurrency /
7 scsi / 7 cost); folded + owner decisions. The dominant convergence (3 lenses)
was the wrap-don't-copy contradiction — resolved in §3.3/§3.6/§5. The single
largest v0.1→v0.2 change is an **owner reversal**: v0.1 §4 "reads accept
shoe-shine" is REJECTED; anti-shoe-shine is now core, designed in §4 as a
watermark-controlled host-RAM read reservoir.
**Naming:** settled — **TIO-6** (commits on main already use it; v0.1 Q5 closed).
**Problem source:** the 2026-07-07b MSL3040 window measured field restore at
**13.28 MB/s** end-to-end (undecomposable at the time — read diag was missing),
while a different read path had done 82 MB/s on the same class of object and
the kernel `st` driver via `dd` sustained **246–293 MB/s on the same drive,
HBA, and cartridge**. TIO-5a/5b then landed the write hot-submitter and the
read-side SAFETY machinery (typed `valid_bytes` handoff, staged-CDB cancel,
fixed-ILI cursor invalidation, reset-UA parity, read diag decomposition).
**R1 — the relay fix — has since LANDED (main@5740f1a)**: the 10 ms
sleep-quantized retry is gone (watchdog-bounded `send_with_timeout`), default
chunk is 256 KiB, the sender channel is sized by a 4 MiB byte budget, and
explicit 4 MiB h2 stream/connection windows are set. What remains is R2: reads
are still **synchronous batched-refill-on-exhaustion** — the drive is idle
while the host parses/hashes/relays, and the host is idle while the drive
reads. This design is the throughput-and-wear half: the read submitter plus
the read reservoir, targeting **300 MB/s** restore (owner decision, §11).
**Related:** `design-tape-io-pipelined-submission-v0.1.md` (frozen v0.7 — §3
is the template this mirrors, §5 is this design's charter, §6 the invariant
set), `report-st-harvest-2026-07-10.md` (F1/F2/F4 read semantics already
folded into TIO-5b; C4/O10 = why rem deliberately does NOT copy st's
kernel-buffer/backspace read-ahead), `memo-field-window-2026-07-07b.md`
(field evidence), `prompt-tio-5b.md` (LANDED main@d2618f7 — the safety code
this design wraps), `prompt-tio6-r1.md` (LANDED main@5740f1a),
`design-tape-io-throughput-v0.1.md` (TIO-1..4 error machinery, untouched).

---

## 1. The gap, decomposed

### 1.1 What the read path does today (main @ 5740f1a — post-R1)

One restore currently runs this chain, **all serialized on the drive-actor
thread** except the final hop:

```
drive-actor thread                              relay thread
──────────────────                              ─────────────
SG_IO READ (1 MiB, 4×256 KiB records)   ─┐
decode outcome (read_block_batch funnel) │  all serial,
copy each 256 KiB record out of the ring │  drive idle from
tar/PAX format parse                     │  ioctl return to
SHA-256 over payload bytes               │  next submit
StagedReadWriter: to_vec + send ─────────┴─►  recv (sync_channel depth 2)
     (blocks when relay is behind)            ChannelWriter: chunk to 256 KiB
                                              watchdog send_with_timeout
                                              (R1: no sleep loop; byte-budget
                                               channel; 4 MiB h2 windows)
                                              tonic/h2 → client
```

Concretely (`crates/remanence-api/src/read_core.rs`,
`crates/remanence-api/src/write_owner.rs::stream_one_object` /
`stream_with_staged_read_sender_diagnostics`):

- `BatchingBlockSource::refill` issues the next batched READ **only when the
  previous buffer is exhausted** — synchronous ping-pong. The TIO-5b ring
  (`free_buffers`) exists for buffer *reuse*, not read-ahead; exactly one
  buffer is ever being filled or drained at a time.
- Between consecutive SG_IOs the actor thread does the format parse, the
  SHA-256 (`CapturePayloadSink` hashes every payload byte), one full-record
  memcpy per record, one `to_vec` per format-emitted slice, and a send into a
  **depth-2 rendezvous channel** that blocks whenever the relay is behind.
  The write path's disease — host work serialized between ioctls (TIO-5 §1)
  — plus: the *network's* backpressure still propagates directly into the
  SCSI submission gap (R1 made the relay fast and honest, not decoupled).
- ~~The relay thread's 10 ms sleep-quantized `try_send` retry~~ — **fixed by
  R1** (main@5740f1a): watchdog-bounded blocking send preserving the
  per-chunk deadline semantics, 256 KiB default chunks, channel sized by a
  4 MiB byte budget, explicit 4 MiB h2 stream+connection windows,
  `sender_stall_ms` in restore diag. Field confirmation = leg 0 (§10).

### 1.2 Why this is the wrong shape for a tape drive

Reads and writes are symmetric at the drive: the drive fills its internal
buffer from tape autonomously (read-ahead is the drive's job; LTO drives
stream ahead of host demand within their buffer), and the host's only
obligation is to **drain at an average rate ≥ the drive's minimum streaming
rate**. Every millisecond of host-side work between ioctls subtracts from
drain rate exactly as it subtracted from feed rate on the write path. The
morning dd battery proved the platform: synchronous 1 MiB reads through the
same E208e sustain 246–293 MB/s when the submission gap is ~0.1 ms (st's
buffered path). rem's read gap today is not ~0.1 ms; it is
parse+hash+copy+relay-blocking — unbounded, because the gRPC consumer sits
inside it.

**Model caveat, stated up front (panel):** the write→read cadence symmetry
(§2-A, §3.1, §7) is an *assumption*, not a measurement. On reads the drive
must stream AHEAD of host demand; a host stall drains the drive's buffer, and
the next READ then waits on **tape**, not on microseconds. Read ioctl cadence
has never been measured physically on this hardware — it is the single
biggest uncertainty in the §7 model, and leg 0/1 measures it before any
number in §7 is treated as a promise.

### 1.3 Three distinct problems — do not conflate them

1. **The submission gap** (R2 core): host work between ioctls. Fixed by the
   read submitter (§3), the mirror of TIO-5 §3.
2. **The relay pathology** (§6): 13.28 vs 82 MB/s was a ~6× gap that no
   submission-gap model explains. H1 (the 10 ms sleep quantum) was the prime
   suspect; **R1 landed the fix**; leg 0 confirms in the field.
3. **Drive wear under slow consumers** (§4, NEW as a first-class problem):
   any consumer whose sustained rate is below the drive's lowest
   speed-matching band forces stop/reposition cycles (shoe-shine). v0.1
   accepted this; the owner rejected that. The read reservoir (§4) is the
   mechanism that converts continuous backhitching into a small number of
   clean park/resume cycles.

The 82 MB/s July figure is itself consistent with sync ping-pong (§7.1);
the 13.28 figure was the relay pathology stacked on top of it.

## 2. Approaches considered

**A — Hot read submitter on the drive actor, strictly one command in
flight, reservoir handoff to a decoupled consumer (ADOPTED).** Feed-rate
arithmetic transfers from the write side *as a model* (see §1.2 caveat):
ioctl ≈ 0.85–1 ms for 1 MiB when the drive buffer has data; inter-command gap
≤ 0.3 ms ⇒ drain capacity ~740 MB/s ⇒ drive-limited (~300 MB/s HH LTO-9) with
2.5× headroom — **if** read cadence matches write cadence, which leg 0/1
verifies. No command overlap needed, ever. st itself is this shape on reads
(one outstanding command; its read-ahead is host-side buffering, `st.c` C4).

**B — Async sg / queue-depth-2 READs (REJECTED).** Same non-cancellable
in-flight hazard as TIO-5 §2-B, and on the read side it is *worse*: a queued
READ dispatched past a FILEMARK/ILI outcome physically crosses a tape-file
boundary or consumes a mismatched record before the host has decoded the
prior outcome — precisely what TIO-5b's staged-CDB-cancel exists to prevent.
Buys ≤1 ms/command that approach A does not need.

**C — st-style kernel-buffer + backspace read-ahead (REJECTED, standing
decision).** st reads ahead speculatively and *backspaces* over unread
records or accidentally crossed filemarks (`st.c` C4/O10). rem's plan-bounded
read-ahead (§3.1) never reads a record the restore plan does not already
name, so there is nothing to backspace over. The st harvest already
documented-discarded this structure; this design re-affirms it. Note the
reservoir (§4) is NOT this: it buffers records the plan names, downstream of
the funnel, with zero speculative motion.

**D — Fix the relay only, keep synchronous refill (REJECTED as
insufficient; its relay fixes were ADOPTED as stage R1 and are LANDED).**
With the relay unblocked and a fast consumer, sync refill's duty cycle is
ioctl/(ioctl + consume) ≈ 3.3/(3.3+0.7–1.5) ≈ 70–83% at 1 MiB/command ⇒
~210–250 MB/s ceiling, below the 300 target, and fragile: any consumer hiccup
lands directly in the submission gap. R1 landed first so the physical
decomposition can attribute the submitter's own contribution honestly (§11).
The cost lens argued for gating R2 on leg-0 measurement; the owner overruled:
see §11.

## 3. Architecture

Three moving parts. The consumer machinery is *relocated and retyped* — the
panel killed v0.1's "reuse BatchingBlockSource verbatim" recipe (§3.3).

```
drive-actor thread             decode thread                 sender thread
(read submitter §3.1,          (§3.3 — HandoffBlockSource:   (§3.3, exists today —
 owns the DRIVE BlockSource;    NO drive access, by type)     drain_staged_read_sender,
 reservoir gate §4)                                           post-R1 mechanics)
┌──────────────────────┐       ┌────────────────────┐        ┌──────────────────┐
│ gate on reservoir    │       │ recv Result<handoff>│       │ recv bytes       │
│  watermarks (§4)     │       │ validate (parity    │       │ chunk + watchdog │
│ pop free buffer      │       │  with refill checks)│       │ send_with_timeout│
│ recompute count      │       │ format parse        │─bytes─►│ (R1, re-scoped  │
│ read_buffer_handoff  │─hand──►│ SHA-256 (verify    │ chan  │  deadline §4.5)  │
│  (THE 5b funnel,     │  off  │  layer, §3.4)       │(byte- │ tonic/h2 → client│
│   unmodified)        │ =res- │ return buffer ──────┼─sized)│                  │
│ push Ok(handoff)     │ ervoir│                     │─free──┼─► (to submitter) │
│ or Err(root) once    │ entry │                     │  chan │                  │
└──────────────────────┘       └────────────────────┘        └──────────────────┘
```

### 3.1 The read submitter (hot loop)

Runs on the drive-actor thread that owns the `DriveHandle` — no new
ownership or locking semantics, mirror of TIO-5 §3.2. **The drive-side
`BlockSource` moves INTO the submitter closure; no other thread can reach the
drive, by construction (§3.3, §5.4).** GOOD-path loop, in full: check the
reservoir gate (§4 — pass-through unless at high-water) → pop a free ring
buffer (blocking only when the pool is exhausted) → **recompute the record
count from post-decode state**:
`records = min(read_batch_blocks_effective, remaining_records_in_plan)`
→ call **`BlockSource::read_buffer_handoff`** (which is `read_block_batch`
— the single READ funnel, timeout class, CDB build, sense decode, position
arithmetic, tripwire, per-command audit events, diag histograms, **all
unmodified**) passing the fresh `remaining` (the funnel's own
`requested.min(remaining_records_in_file)` clamp at `tape_io/mod.rs:1342`
remains the backstop) → push the typed `Ok(ReadBufferHandoff)` into the
delivery channel (never blocks, §3.2) → decrement `remaining_records_in_plan`
by `records_read` → repeat until the plan is exhausted.

- **Plan-bounded read-ahead:** the total records to read are known before
  the window opens (`tape_file.block_count` from the catalog for full-object
  reads; `plan.block_count` for ranged reads). The submitter reads ahead of
  the *consumer*, never ahead of the *plan*. No speculative record is ever
  issued, so approach C's backspace machinery stays unnecessary by
  construction.
- **Staged-next carries the BUFFER only — the count is recomputed before
  EVERY issue** (panel: concurrency #1; v0.1's "the next buffer and record
  count are already in hand" and §5.2's "armed intent survives as-is" are
  DELETED — carrying a count across even a full-count outcome over-reads the
  plan tail and violates the golden fixture). `StagedRead { buffer }` is host
  memory only; `records` is derived from `remaining_records_in_plan` at issue
  time, every time. The CDB is rebuilt per command
  (`build_read_fixed_cdb(records)`), same rule as the write side's
  partial-batch CDB rebuild. Because the staged READ never reaches the
  kernel early (approach B rejected), **cancellation is control flow and is
  always possible**: see §5.2. Hermetic assertion:
  `requested_records ≤ remaining_after_decode` on every issue (§10).
- **How read-ahead coexists with exactly one command in flight:** the
  submitter thread is *blocked inside* the SG_IO for the duration of each
  command — there is never a second command, staged or otherwise, at the
  transport. Read-ahead is achieved purely by what the submitter does NOT
  do between ioctls: no parse, no hash, no copy, no channel rendezvous with
  the network. On completion, the next buffer is in hand and the count is
  one subtraction away, so the next READ is issued within microseconds. The
  drive's own buffer, already filled ahead from tape, satisfies it
  immediately. Cadence ≈ ioctl + ε on ordinary commands; on tripwire-RP
  commands the cadence includes the periodic inline READ POSITION (a known,
  bounded, ~ms cost — stated so the cadence claim is not glossed). **This
  cadence model is write-side-derived and unverified for reads (§1.2) —
  leg 0/1 measures it.**
- Position seeding, the 1 GiB drift tripwire, and the 900 s TapeIo timeout
  class all live inside the funnel already
  (`seed_expected_position` is arithmetic when the cursor is cached;
  `advance_expected_position` fires the inline tripwire RP) — the submitter
  adds nothing and removes nothing.

### 3.2 Reservoir pool, channels, error-carrying delivery

Mirrors TIO-5 §3.1 with the buffer flow reversed (empty buffers flow to the
submitter; filled, typed handoffs flow away from it), with two v0.2 changes:
the buffer pool scales into the **read reservoir** (§4), and the delivery
channel **carries errors**:

- **Pool/reservoir:** the working set of `effective_read_batch_blocks ×
  block_size` buffers (1 MiB today: 4×256 KiB under the sg 1 MiB grant),
  page-aligned, checked allocation. Minimum depth = `staging_ring_buffers`
  (existing key, default 4, validated 2..=16; `BlockSource::read_ring_buffers`
  already plumbs it); maximum depth = the reservoir byte budget (§4.6–4.8),
  allocated incrementally on demand. Effective pool bytes logged at window
  open (write-side parity). Read and write sessions are mutually exclusive
  per drive, so the ring-depth key doubles no budget; the reservoir draws
  from the shared daemon RAM budget (§4.6).
- **Free channel:** capacity ≥ pool size, pre-seeded/grown with the pool.
  Buffer return (from decode thread, `into_reusable_buffer`) is therefore
  always non-blocking — the write side's no-credit-loop-wedge rule.
  **Construction assertion (panel nit):** at window open and after every
  growth step, `allocated_buffers == free_capacity_headroom ==
  delivery_capacity_headroom` — the non-blocking-push proof (below) is
  guarded by this assertion, not assumed from config.
- **Delivery channel:** bounded, capacity = pool size, element type
  **`Result<ReadBufferHandoff, TapeIoError>`** (panel: error precedence,
  §5.6). The submitter can hold at most `pool` buffers across in-flight +
  queued, so its `Ok` push never blocks — asserted per the construction
  assertion. If the push fails (receiver dropped = consumer died), the
  submitter stops issuing immediately: **no tape motion for a dead client**,
  bounded by the one command already in flight.
- **Ownership handoff:** `ReadBufferHandoff` (TIO-5b's type) moves the
  buffer by value; `valid_bytes`/`records_read`/terminal flags travel with
  it. Fail-closed funnel outcomes return `Err` and never surface a buffer —
  unchanged. The stale-tail property (sentinel-prefill test) is unchanged
  because the exposure surface — the typed handoff — is unchanged; §4.4
  adds the `bytes_transferred` cross-check because reservoir buffers now
  live far longer across reuse, which *amplifies* stale-tail exposure if a
  residual ever over-reports.
- **Terminal poison protocol** (mirror of TIO-5 §3.1 / pool_write.rs
  1568-1588, roles reversed): on any error the submitter (a) stops issuing,
  (b) sends `Err(classified_root_error)` down the delivery channel
  **exactly once** (or poisons the shared flag if the channel is gone),
  (c) drains the free channel without issuing CDBs. **Drop/join order
  (specified, panel minor):** submitter closes the delivery sender → decode
  thread drains remaining `Ok` items, sees the close (or the `Err`), drops
  its sender to the sender-thread channel → sender thread unwinds → joins in
  reverse spawn order. A decode thread parked in a blocking recv is always
  released by the channel close. **Stop detection is symmetric (panel
  minor):** the submitter treats *either* a delivery-push failure *or* a
  free-channel recv disconnect as consumer death — decode death is noticed
  on whichever channel the submitter touches first.

### 3.3 Consumer side: decode thread + sender thread (relocated AND retyped)

The two-thread restore relay split already exists
(`stream_with_staged_read_sender_diagnostics`: produce closure + sender
thread). Today the produce closure runs on the drive-actor thread. v0.1
proposed "reuse `BatchingBlockSource` with `refill`'s SCSI call swapped for
`recv()`, everything else byte-for-byte." **The panel killed that recipe
(SCSI-B1 + concurrency #2/#3, convergent):** `BatchingBlockSource` holds
`inner: &mut dyn BlockSource` — a live drive reference — so the "swap one
call" recipe puts a drive handle on the decode thread, making one-in-flight a
convention instead of a construction, and `refill` conflates
clamp/alloc/issue/validation, hiding a second divergent copy of the plan
state. v0.2:

- **`HandoffBlockSource` is a DISTINCT type holding `{delivery_receiver,
  free_sender, block_size, remaining}` and NO drive reference.** It cannot
  issue SCSI, by type. The drive-side `BlockSource` moves into the submitter
  closure (§3.1); the borrow checker enforces exclusivity.
- **A narrow read-only sub-trait for the format consumer.** `BlockSource`'s
  motion methods (`locate`/`space`/`position`) are *required trait members*
  — v0.1's "compile-time removal" on the full trait was inaccurate (the
  honest alternative was `unreachable!()` runtime panics). Instead, split
  the trait: `BlockRead` (`read_block`, `read_block_batch` surface, block
  geometry) as a super-trait or sibling of `BlockSource`, and the format
  layer's streaming entry points take `&mut dyn BlockRead`. Motion methods
  are absent from the decode thread **by type** — now genuinely
  compile-time. (Existing full-`BlockSource` callers are unaffected;
  `BlockSource: BlockRead`.)
- **Decode-side `remaining` is slaved, never recomputed:**
  `remaining = plan_total − Σ handoff.records_read`, decremented only by
  received handoffs. There is exactly one *authoritative* plan cursor (the
  submitter's); the decode-side counter is a derived checksum of it, and a
  mismatch at window close (Σ received ≠ Σ issued) is a fail-closed error,
  not a silent divergence.
- **Validation parity is a test obligation, not a code-reuse claim:** the
  consumer-side checks that today live in `refill` — the byte/record
  mismatch check, the filemark-before-plan-end fail-closed error,
  `read_block`'s per-record copy-out — are reimplemented on
  `HandoffBlockSource` and pinned by parity tests that assert byte-identical
  error behavior against today's `refill` (§10).
- The existing **sender thread** (`drain_staged_read_sender` →
  `ChannelWriter`) is retained with R1's landed mechanics; the per-chunk
  deadline is re-scoped by §4.5.
- **The decode→sender channel is sized in BYTES, and stated (panel:
  concurrency #4):** today's depth-2 rendezvous `sync_channel` re-couples
  SHA/parse to network jitter, undercutting the split. v0.2: byte-budgeted
  bounded channel (default 4 MiB, same budget arithmetic R1 landed for
  sender→tonic in `read_stream_channel_capacity`). The v0.1 Q4 question
  ("does the sender split still pay?") is contingent on this sizing and
  stays deferred-to-measurement.
- The format parse, hashing, and copies now run **concurrent with the next
  SG_IO** instead of inside its gap. Consumer budget at 300 MB/s: one
  256 KiB copy-out per record + SHA-256 (~1.5–2 GB/s with SHA extensions —
  **dev box CONFIRMED `sha_ni` 2026-07-11 (Ryzen 3700X); the DL385 EPYC is
  architecturally ≥Zen1 and therefore has it, one-line `grep sha_ni
  /proc/cpuinfo` confirm at leg 0 before the number is trusted**) + one
  `to_vec` per emitted slice ≈ 25–40% of one core (§7.2). If a future
  profile shows the decode thread itself binding, the split point between
  decode and sender can move — measurement-triggered follow-up, not R2
  scope.

### 3.4 Layering: transport ⊥ verification (owner decision)

Stated as a design principle because it decides where code lives and
reinforces the no-drive-on-decode-thread construction:

- **The transport (submitter + reservoir + sender) is FORMAT-BLIND.** It
  moves opaque, proven-complete records; it never touches SHA, never parses,
  never knows what a RAO object is. Its integrity obligations are exactly
  the funnel's: typed `valid_bytes`, the §4.4 cross-checks, position proof.
- **Verification lives in the format/decode layer**, using the RAO object's
  **self-describing in-band manifest checksums — read from the object on
  tape, NEVER from the catalog.** The catalog *locates* (which tape, which
  file number, how many blocks); it is never the integrity reference. A
  restore therefore verifies against what the tape itself claims to
  contain, end-to-end, with zero catalog trust in the data path.
- **A different-format plugin brings its own decode-layer verification (or
  none); the transport is unchanged.** This is the plugin seam: format
  drivers get `&mut dyn BlockRead` + their own manifest semantics.
- **SHA-256 stays inline on the decode thread** (v0.1 Q2, owner-confirmed):
  hash-at-restore is the right default for an archive; the budget holds
  with SHA-NI (§3.3); a silent restore-without-verify mode is not offered.

### 3.5 Session integration

`stream_one_object` / `stream_one_file_range` keep their signatures and
catalog work. **Positioning state is per-MOUNT, not per-object (panel,
convergent):** `verify_loaded_tape_identity` (rewind + BOT bootstrap check)
runs once per mount/session, not per restored object; per-object work within
a verified mount is SPACE/LOCATE to the tape file only. The pipelined window
covers the record-transfer phase of each object, mirroring the write side
(position seeding and motion never prep-ahead). v0.1's §7 model was
single-large-object; production restores are "pull N clips" — per-object
positioning + window spawn/join then dominates exactly as it ate the write
field number (~65 s/object). v0.2 therefore: (a) adds a **multi-object
acceptance leg** (§10 leg 4) so the shipped number is a restore-workload
number, not a single-object best case; (b) **names the follow-up arc
honestly:** amortizing inter-object positioning (position-ordered restore
plans, LOCATE vs SPACE selection, batched inter-object motion) may be worth
more than the submitter on clip-pull workloads — out of R2 scope, named in
INDEX as the next candidate arc.

Ranged reads (`read_plaintext_file_range` / `stream_one_file_range`) are
**reworked onto the same pipeline** — see §5.5; v0.1 had left them as a
second synchronous consumer, which falsified the one-path claim. CLI
break-glass reads (`rem-debug archive read`) route through the same core
with a file-writer consumer; there is exactly ONE read transfer path after
this lands (v0.7 one-path rule — no `read_pipeline` flag, no legacy refill
mode kept; backout is git revert + previous binary). Mid-object restore
resume does not exist and is not designed here (panel nit — named): a failed
restore restarts from object start; the reservoir's volatility (§4.7) is
consistent with that.

### 3.6 Diag restructure (decomposition under overlap)

TIO-5b's restore decomposition assumes serial phases
(`relay = wall − position − transfer`, `exclusive_restore_relay_phase`).
Under overlap that subtraction is meaningless — transfer and relay run
concurrently. The diag moves to per-thread busy/idle accounting:

- Submitter: existing `gap_us`/`ioctl_us` histograms (recorded in the
  funnel, **unchanged**) plus new pipeline-side `free_wait_us` (blocked on
  the free channel) and `park_us` (reservoir gate, §4). **`gap_us` will now
  include deliberate waits** (free-wait, park) because the funnel measures
  completion→next-submit and cannot know why the gap happened; the pipeline
  therefore also records a derived `feed_gap_us = gap − free_wait − park`
  per iteration, and **acceptance gates read `feed_gap_us`** (panel minor:
  no double-counting, no polluted feed-health signal). `free_wait_us` high ⇒
  consumer-bound; `park_us` accumulating ⇒ sub-band consumer (§4).
- Decode thread: busy time (parse+hash) vs recv-wait (drive-bound signal).
- Sender: busy vs channel-wait, plus R1's landed `sender_stall_ms`.
- **Per-command audit events are RETAINED (panel BLOCKER resolved):** v0.1
  §8's coalesced TapeRead window span required either modifying
  `read_block_batch` (which fires `fire_tape_started`/`finish_tape_success`
  per command, mod.rs:1359/1392) or forking an audit-free
  `read_block_batch_pipelined` — the exact ~200-line fork that produced
  TIO-5a's six defects. The audit hook is `None`/unwired in production and
  a per-command event costs nanoseconds. **Coalescing is DROPPED.** The
  funnel is wrapped truly unmodified. If forensic coalescing is ever wanted,
  the rule is: extract an audit-free CORE called by both entry points —
  never a fork. The window-open intent marker (planned CDB count/bytes,
  write-side forensic parity) is kept — it is emitted by the pipeline,
  outside the funnel.
- **Three threads now write diag concurrently (panel minor):** counters and
  histograms shared across threads are atomics, or thread-local and merged
  at post-join snapshot; no torn reads in the restore_total line.
- The restore_total diag line keeps its fields; `relay_ms` becomes
  sender-busy, and a new `bottleneck=` field names the thread with the
  highest busy fraction. New reservoir gauges: occupancy bytes (live),
  park/resume cycle count, regime (§4). **A live signal, not just
  session-close accounting (panel minor):** reservoir occupancy + park
  count are readable mid-session from shared diag state and feed the
  Drishti signal (§8).
- All in-place counters/histograms, zero per-command allocation, read from
  shared diag state, never via a `DriveCommand` (actor busy for the window —
  unchanged known property).

## 4. The read reservoir and the anti-shoe-shine controller (owner reversal — CORE)

**v0.1 §4 said "reads accept shoe-shine." The owner REJECTED that.** The
throughput lens had already shown the dismissal was on the wrong grounds:
host buffering cannot raise sustained throughput above the consumer's rate
(still true, kept below), but it directly cuts **reposition frequency** —
which is the wear term. And "restores are rarer than ingests" is wrong for
this archive's access pole: a newsroom restore over 1GbE would shoe-shine
continuously for the whole session. Anti-shoe-shine is now a core mechanism
of R2, not an accepted defect.

### 4.1 The mechanism: a watermark-controlled host-RAM read reservoir

The buffer pool of §3.2 scales from a 4-buffer jitter ring into a large,
byte-budgeted **read reservoir** with configurable **high/low watermarks**
(rem config keys, §4.8):

- **Fill:** the submitter runs at full drive speed into the reservoir
  (reservoir occupancy = filled, not-yet-consumed handoff bytes).
- **At high-water: STOP issuing READs.** The drive, seeing no host demand,
  parks cleanly — **one** deliberate stop, instead of continuous
  backhitching at the speed-matching band floor.
- **Drain:** the decode/sender threads keep consuming from the reservoir at
  the consumer's own pace. The drive sits parked, wearing nothing.
- **At low-water: resume** — a single reposition + position re-proof
  (§4.3), then full-speed fill again.

**Wear arithmetic (the point):** park/resume cycles per restore ≈
`restore_bytes ÷ (high − low watermark span)`. The owner's example: a 100 GB
reservoir against a full 18 TB LTO-9 stream ≈ **180 clean stop-starts** for
the entire tape — versus continuous shoe-shining for the whole multi-hour
session without it. Typical clip-pull restores are far smaller than a full
tape, so typical cycle counts are single digits. The knob is RAM, which is
cheap and reusable; the thing it buys is head/media life, which is not.

**What the reservoir cannot do (unchanged honesty from v0.1):** sustained
end-to-end throughput is still `min(chain)` — a 110 MB/s consumer gets
110 MB/s. The reservoir changes the drive's *duty pattern* (long full-speed
bursts + parks instead of sub-band crawl/backhitch), not the average.

### 4.2 Two regimes, one control law

The same reservoir serves both poles of the archive's identity:

- **Fast consumer (≥ drive streaming rate):** the reservoir stays near
  empty; read-ahead keeps the drive streaming at native rate (→300 MB/s).
  Watermarks never trip. This is the §3 submitter story, unchanged.
- **In-band consumer (within the drive's speed-matching range, roughly
  ⅓–1× native on HH LTO-9):** **hardware speed-matching first (owner).**
  At high-water the submitter gates *per command* — it issues the next READ
  as soon as consumption frees space below high-water. Sustained submitter
  demand therefore converges to the consumer's rate; the drive's own
  speed-matching steps tape speed down to match, and the drive streams
  continuously with zero parks. The reservoir hovers at high-water and
  absorbs band-granularity jitter (the drive's internal buffer absorbs the
  rest).
- **Sub-band consumer (below the drive's lowest speed-matching band):** the
  drive cannot go slow enough; without intervention it backhitches
  continuously. This is the **stop-start batching fallback**: at high-water
  the submitter stops entirely and waits for **low-water** before resuming
  (full hysteresis span), producing the §4.1 park/drain/resume cycle.

**Unified control law:** pause at high-water, resume at a threshold — where
the resume threshold is `high-water − ε` (in-band: effectively per-command
gating) or `low-water` (sub-band: full hysteresis). Regime selection: an
EWMA of consumer drain rate compared against the configured drive floor
(`read_drive_floor_mib_s`, §4.8), with switch hysteresis (enter sub-band
below `floor × 0.9` sustained ~10 s; return above `floor × 1.1` sustained
~10 s) so a consumer hovering at the floor doesn't flap regimes. The floor
default is conservative (~⅓ native ≈ 100 MiB/s for HH LTO-9); **leg 3
qualifies the real floor for this drive** and the default is corrected from
measurement.

### 4.3 SAFETY: park and resume never weaken position trust

- **Pauses happen only at command boundaries with a fully classified prior
  outcome.** The gate sits at the top of the submitter loop (§3.1), before
  buffer pop and count recompute — never mid-command, never between a
  CHECK CONDITION and its decode. **A pause can never straddle a filemark:**
  reads are plan-bounded within one tape file; a FILEMARK outcome mid-plan
  is already a fail-closed error (§5.1) and ends the window — there is no
  state in which the submitter parks with an unresolved boundary.
- **Position is RE-PROVEN on resume.** After any pause in which the drive
  may have parked/repositioned (rule: any submitter pause — gate or
  free-wait — longer than `T_reproof`, a constant, default 250 ms), the
  first action on resume is an explicit **READ POSITION**, verified against
  the expected cursor via the existing `DevicePositionProof` machinery,
  BEFORE the next READ is issued. Mismatch ⇒ `mark_position_unknown()` +
  poison — identical to the tripwire path. An RP costs ~1 ms against a
  multi-second reposition, so the conservative trigger is essentially free.
  The 1 GiB drift tripwire continues unchanged on top of this; park re-proof
  is additional, not a replacement.
- The drive's own reposition on resume is the drive's business (it re-ramps
  and relocates to where its buffer left off); rem's obligation is exactly
  the re-proof above — trust nothing across a park that has not been proven.

### 4.4 INTEGRITY: only proven-complete records enter the reservoir

Reservoir entry *is* the typed handoff push — there is no other door:

- A buffer enters the reservoir only inside an `Ok(ReadBufferHandoff)` whose
  `valid_bytes`/`records_read` came from the funnel's sense decode.
  Fail-closed outcomes never surface a buffer (TIO-5b, unchanged).
- **NEW — `bytes_transferred` cross-check (panel, convergent, funnel
  hardening):** today the funnel destructures `bytes_transferred` from
  `CheckCondition` (mod.rs:1400-1402) and then derives `records_read` from
  the sense INFORMATION residual alone on the filemark and recovered paths.
  An over-reported residual would mint `valid_bytes` beyond what the
  transport actually moved — and under reservoir-scale buffer reuse that
  exposes a PRIOR window's real tape data, strictly worse than the sentinel
  case. v0.2 requires, on every residual-derived success path:
  `records_read × block_size ≤ bytes_transferred`, **fail-closed
  (completion-unknown) on disagreement**, with a hermetic test. This lands
  **inside the one funnel** (`read_block_batch`) as a prerequisite hardening
  commit that benefits every caller — it is a shared-funnel safety fix, not
  a pipeline fork (the write path gets the symmetric check in the same
  commit).
- **NEW — impossible-residual fail-closed (panel SCSI-M4, st O7):** a
  RECOVERED ERROR with **no** terminal flag but a VALID nonzero residual is
  physically incoherent for fixed-mode reads — trusting it as a short read
  advances the cursor BEHIND the physical tape, and read-ahead then delivers
  wrong-position data. v0.2: on the recovered-no-terminal-flags path,
  require residual == 0; VALID nonzero residual ⇒ completion-unknown/poison.
  The current `unwrap_or(records)` (mod.rs:1453) remains only for the
  VALID-bit-unset case. Same funnel-hardening commit; §5.1 row updated.
- Same commit, same rationale (pre-existing, panel m6):
  `is_reset_unit_attention` covers 06/29/00..04 but not 29/07 (I_T NEXUS
  LOSS), which is equally state-invalidating — added.

### 4.5 TRUST: liveness ≠ slowness (panel convergent on R1, resolved)

The panel caught v0.1's 30 s send-deadline contradicting its own
slow-consumer story: a 1GbE client pausing >30 s (GC, flush) got killed,
while a genuinely dead peer was invisible to a send-stall timer anyway.
Owner decision resolves it:

- **Slow-but-alive ⇒ park and wait. Indefinitely.** The reservoir + parked
  drive means a stalled-but-connected client no longer costs tape motion or
  submission-gap pollution — it costs RAM (already reserved) and the drive
  *reservation*. No automatic abort for alive peers.
- **DEAD peer ⇒ abort via h2/TCP connection state, NOT a send-stall
  timeout.** Detection chain: client disconnect (RST/FIN or RPC cancel)
  drops the tonic response future → the read-stream receiver drops → R1's
  landed receiver-drop watchdog already converts a blocked sender into an
  immediate `BrokenPipe`. For half-open TCP (client power loss, no FIN),
  the daemon enables **HTTP/2 keepalive PING** (`http2_keepalive_interval` +
  `http2_keepalive_timeout`, values in `reference-configuration.md`) so the
  connection — and with it the stream, the receiver, and the session — dies
  within interval+timeout regardless of send-stall state.
- **R1's 30 s per-chunk `send_with_timeout` deadline is RE-SCOPED in R2:**
  it stops being a client-kill policy and becomes a diagnostic tick — on
  expiry the sender records the stall (Drishti signal, §8) and re-arms; it
  aborts only on channel closure (`BrokenPipe`), i.e. on proven death. (R1
  as landed keeps the 30 s abort in the interim — acceptable pre-R2 because
  without a reservoir a stalled client pins the drive, which is the worse
  evil; the re-scope ships with R2, where parking makes patience safe.)
- **Honest occupancy consequence:** a parked restore holds the drive
  reservation for as long as the client stays alive. The remedy is
  operator-visible (Drishti slow-restore alert + live reservoir/park gauges)
  and operator-actuated (cancel the session). Pre-production rule: no
  auto-eviction machinery, no idle-policy knobs — if the field shows
  parked-forever sessions are a real operational problem, that is a future
  policy design with its own review.

### 4.6 RAM guardrails — never OOM, never swap

- **Budgeted, not aspirational:** effective reservoir size at window open =
  `min(read_reservoir_bytes, MemAvailable − os_headroom, memlock allowance)`
  — reusing the TIO-5b budget-accounting pattern (the
  `daemon.spool_tmpfs_ram_budget` semaphore machinery in
  `remanence-api/src/lib.rs`, generalized: append spool reservations and
  read reservoir reservations draw from **one shared daemon I/O RAM
  ceiling**, so restore + concurrent append can never jointly blow the
  box). Clamp-and-warn (loud, in the window-open log line and diag) when
  the configured size exceeds what is safely available; **refuse to start
  the window** only when even the minimum pool (`staging_ring_buffers`
  buffers) cannot be allocated.
- **Incremental allocation with re-check:** reservoir slabs are allocated
  on demand during fill (checked allocation, page-aligned), and
  `MemAvailable` is re-consulted at growth steps — if headroom would be
  violated, growth stops and the current size becomes the effective cap
  (warn once). Never a single up-front multi-GB allocation spike.
- **Resident, non-swappable:** slabs are `mlock`ed as allocated. A swapped
  reservoir defeats its purpose (drain-rate collapse at exactly the wrong
  moment) — the deployment requirement is `LimitMEMLOCK` on the daemon
  unit (documented in `reference-configuration.md`). If `mlock` fails, the
  reservoir clamps to the minimum pool with a loud warning — a big
  swappable reservoir is worse than a small locked one.
- **Per-drive, per-stream:** each concurrent restore stream owns its own
  reservoir; dual-drive concurrent restore = 2× reservation, both drawn
  from the shared ceiling — integrating the TIO-5 dual-drive RAM budget
  question rather than duplicating it. If the second stream cannot reserve
  its configured size, it clamps-and-warns down to what the ceiling allows
  (never below the minimum pool).

### 4.7 Volatile-is-safe

The reservoir is process RAM, deliberately: a crash/kill loses buffered
records, the session dies, the client sees a stream reset, and the re-issued
restore re-reads from tape. Reads are non-mutating; there is **zero data
risk** in losing the reservoir, therefore **no persistence, no journal, no
spool file** — restore-to-spool was v0.1's rejected strawman and stays
rejected. Crash rows in §9.

### 4.8 Config keys (documented in `reference-configuration.md`)

```toml
[tape_io]
staging_ring_buffers        = 4        # existing; now the reservoir's MINIMUM pool
read_batch_blocks           = 16       # existing, unchanged semantics
read_reservoir_bytes        = "4GiB"   # NEW: reservoir cap per restore stream
read_reservoir_high_pct     = 90       # NEW: stop-issuing threshold (% of effective cap)
read_reservoir_low_pct      = 25       # NEW: sub-band resume threshold (% of effective cap)
read_drive_floor_mib_s      = 100      # NEW: lowest speed-matching band (leg-3 qualified)
```

Validation, fail-closed at config load: `0 < low_pct < high_pct ≤ 100`
(degenerate `high ≤ low` REJECTED, per owner), `read_reservoir_bytes ≥`
minimum pool bytes, `read_drive_floor_mib_s > 0`. Sizing guidance in the
reference doc: parks-per-restore ≈ restore_bytes ÷ hysteresis span; the
4 GiB default is deliberately modest (RAM safety on small hosts) — restore-
heavy deployments with slow consumers SHOULD raise it (the owner's 100 GB
example yields ≈180 parks per full 18 TB tape); the Drishti reposition-rate
signal (§8) is the tell that a deployment needs a bigger reservoir.

## 5. Safety composition — one funnel, wrapped, never duplicated

The submitter calls `read_buffer_handoff` → `read_block_batch` —
**unmodified by the pipeline**. Every TIO-5b behavior holds because the code
that implements it is the code that runs. Two clarifications sharpen this
from v0.1: (a) per-command audit events are RETAINED (§3.6) — the funnel is
not forked for coalescing; (b) the §4.4 hardening items
(`bytes_transferred` cross-check, impossible-residual, 29/07) land **inside
the shared funnel** as a prerequisite commit for all callers — a safety fix
to the one funnel is not a fork of it. What pipelining adds is *what the
submitter does with each classified outcome*:

### 5.1 Outcome table

| Funnel outcome (TIO-5b + §4.4 hardening) | Funnel does | Submitter consequence (new) |
|---|---|---|
| GOOD, full batch | cursor advanced arithmetically; tripwire RP when due; diag recorded | push `Ok(handoff)`; decrement plan; recompute count; issue next |
| GOOD batch, **inline tripwire RP FAILS** (panel: row was missing) | `mark_position_unknown()`; `Err` — the batch's data is NOT surfaced (fail-closed, position unproven) | staged buffer discarded; poison protocol; no further data CDB without reposition + proof |
| Current RECOVERED ERROR, no terminal flags, **residual == 0 or VALID unset** (st F2 — the common case is a FULL batch, panel nit n7) | success; `valid_bytes` = proven-complete records; audited-as-recovered | push `Ok(handoff)`; decrement plan by actual `records_read`; recompute count; continue |
| Current RECOVERED ERROR, no terminal flags, **VALID nonzero residual** (§4.4 — physically impossible for fixed reads, st O7) | **completion-unknown `Err` (NEW, fail-closed)** | poison protocol; stop |
| FILEMARK + residual | `Ok` with `filemark=true`, `records_read` from sense INFORMATION **cross-checked vs `bytes_transferred` (§4.4)**; cursor advanced incl. the mark | staged buffer discarded. Plan not exhausted ⇒ truncated object ⇒ fail-closed error to consumer (exactly today's `refill` rule); no further CDBs. A filemark can never be crossed by a stale staged READ because the staged READ was never in the kernel |
| Fixed-mode ILI (st F4) | `mark_position_unknown()`; `Err`; handoff withheld | staged buffer discarded; poison protocol; **no further data CDB is even possible** — `ensure_data_command_state_valid` refuses until explicit reposition + device position proof |
| Reset-UA `06/29/xx` incl. **29/07 I_T NEXUS LOSS (§4.4)** (st F1) | `invalidate_for_reset_unit_attention()` (cursor + cached mode validation); `Err` | same as ILI, plus mode re-verification required before any data command — enforced by the funnel's gate, not by submitter discipline |
| Deferred sense 0x71/0x73 | completion-unknown `Err` | poison protocol; stop |
| Transport error / undecodable | `Err` via `map_scsi` | poison protocol; stop |
| Tripwire RP mismatch (periodic or §4.3 park re-proof) | `mark_position_unknown()`; `Err` | poison protocol; stop |

### 5.2 The staged-READ-cancel interaction, made explicit

The tricky case the charter names: a pre-staged READ that must be cancelled
because the *prior* command's outcome (residual, filemark, ILI, error)
invalidates its parameters. Design (tightened per panel concurrency #1):

- `StagedRead` carries the **buffer only**. The record count is recomputed
  as `min(batch, remaining_records_in_plan)` from post-decode state before
  **every** issue — full-count outcomes included. There is no "armed intent
  survives as-is" fast path (v0.1 wording deleted): carrying a count across
  the plan tail requests `batch` where `remaining < batch`, and while the
  funnel's own clamp (mod.rs:1342) would rescue correctness when passed
  fresh `remaining`, the design does not lean on the backstop — the
  submitter passes fresh values and asserts them.
- **Invalidation rule:** any funnel return where
  `records_read != records_requested` **or** any terminal flag is set
  **or** `Err` ⇒ the staged buffer is discarded/recycled *before* any issue,
  and `remaining_records_in_plan` / clamp are recomputed from post-decode
  state (`records_read`, cursor validity).
- Because approach B was rejected, the staged READ exists only as host
  memory: cancellation cannot fail, cannot race the transport, and needs no
  kernel cooperation. This is TIO-5b's staged-CDB-cancel machinery promoted
  from a single-refill property to a loop invariant, with hermetic
  assertions (§10): **no READ CDB is ever issued whose record count exceeds
  the post-decode remaining plan — under every outcome in the matrix,
  including all-GOOD tails.**

### 5.3 New failure surfaces introduced by pipelining (owned honestly)

- **Consumer death mid-window** (client disconnect, decode panic): delivery
  push fails or free-channel recv disconnects (§3.2, symmetric detection) ⇒
  submitter stops after the in-flight command; poison; drain; the
  drive-actor returns to its command loop. Bounded over-read: ≤ reservoir
  buffers already filled + 1 in flight — all discarded, no delivery. Test
  row, §10.
- **Submitter death / process kill:** reads are non-mutating; there is no
  fence, no journal, no commit exposure. Recovery is the ordinary readiness
  path (position distrust on reopen). TIO-5b's chaos row (drop-side never
  issues a destructor READ) extends to the full three-thread scope. Crash
  table, §9.
- **Poisoned-window buffer leak:** pool accounting (allocated == returned
  or dropped-with-window) asserted at window close, mirroring the write
  side's `RingAccounting` imbalance check — now covering incremental
  reservoir growth (§4.6).
- **Parked-drive surfaces (NEW with §4):** position staleness across a park
  — covered by resume re-proof (§4.3); RAM reservation held by a parked
  session — covered by budget accounting + Drishti visibility (§4.5/§4.6);
  a park that never resumes (client alive, zero progress) — operator-visible
  and operator-cancelled, no auto-eviction (§4.5).
- **Abort latency:** operator abort cannot interrupt a mid-ioctl submitter
  (known TIO-3/5 property, unchanged); with 900 s TapeIo timeouts a hung
  READ occupies the actor for up to 900 s. A *parked* submitter (§4), by
  contrast, is abortable immediately — parking improves abort latency in
  the slow-consumer case. Stated for the record.

### 5.4 Motion fencing — by type

During a pipelined window the submitter owns the drive exclusively (it IS
the drive-actor thread, and the drive-side `BlockSource` lives inside its
closure). The decode thread's `HandoffBlockSource` + the format layer's
`&mut dyn BlockRead` surface (§3.3) make motion methods **absent by type**
— v0.1's "compile-time removal" claim is now actually true, because the
narrow trait exists rather than pretending required trait members away.
Ranged reads do their SPACE (filemarks + blocks) before the window opens,
on the actor thread, as today.

### 5.5 Ranged reads — same transport, bounded deliver-ahead (panel convergent)

v0.1 left `read_plaintext_file_range` as a second, synchronous consumer with
**no integrity check at all** (no hash; only the 1 GiB arithmetic tripwire
bounds how far an undetected position error could stream) — and read-ahead
would have *widened* that window to reservoir scale. v0.2:

- **Transport unified:** ranged reads ride the same
  submitter/reservoir/decode/sender pipeline (`HandoffBlockSource` + range
  framing in the decode layer). The one-path claim is true again; the
  second synchronous consumer is deleted.
- **Integrity, honestly bounded:** the RAO in-band manifest (§3.4) carries
  whole-file checksums; a byte-range has no per-range digest today, so
  ranged reads remain hash-less at the payload level. For hash-less reads
  the pipeline **bounds deliver-ahead past unproven position**: bytes are
  released to the sender only up to the last device-proven cursor (proof =
  window-open RP, periodic tripwire RP, park-resume RP), and the proof
  cadence for ranged windows is tightened (RP every
  `position_check_bytes_ranged`, default 256 MiB — vs 1 GiB — chosen so the
  proof cost stays noise while the unproven-delivery window shrinks 4×).
  Records between the proven cursor and the read cursor sit in the
  reservoir, undelivered, until the next proof lands. Full-object reads are
  exempt from the release bound — their end-to-end manifest hash catches
  wrong-position data with certainty at delivery.
- **Upgrade path, named:** if/when the RAO format grows per-chunk digests,
  the decode layer verifies ranges directly and the release bound retires.
  Format question, tracked in §12, not R2 scope.

### 5.6 Error precedence across three threads (panel convergent)

Without a rule, a submitter-side SCSI error surfaces to the client as a
derived "channel closed" from whichever thread noticed last — gutting
attributability. The rule:

- The delivery channel carries `Result<ReadBufferHandoff, TapeIoError>`
  (§3.2); the submitter's classified root error travels **in-band, exactly
  once**.
- **Precedence: submitter root error > decode-derived error > sender/
  transport error** — except a genuine client disconnect (h2/TCP-proven,
  §4.5), which short-circuits as its own terminal cause (there is no client
  to attribute to).
- **Single emitter:** the sender thread is the sole writer of the terminal
  gRPC status; decode folds any in-band `Err` it receives into its
  downstream channel unchanged; concurrent poison sources (submitter error
  + decode panic + sender stall racing) resolve to the highest-precedence
  cause via a once-cell terminal slot, asserted in tests (§10).

## 6. The 13.28 MB/s pathology — hypothesis ladder (R1 LANDED)

The 82→13.28 gap was ~6×; no submission-gap model explains it. Ranked
hypotheses, updated for R1 (main@5740f1a):

| # | Hypothesis | Status post-R1 |
|---|---|---|
| **H1** | **`ChannelWriter`'s 10 ms sleep-quantized retry** — 64 KiB chunks against a 16-deep channel locks the writer to 1–2 chunks per 10 ms ⇒ 6.5–13 MB/s, bracketing the field number | **FIX LANDED** (watchdog `send_with_timeout`, no poll quantum) + `sender_stall_ms` observable. Mechanism-confirmed; field confirmation = leg 0 |
| **H2** | h2 flow-control window at default 65,535 B — ~13 MB/s at ~5 ms RTT independently | **FIX LANDED** (explicit 4 MiB stream + connection windows). Client side must match — cross-repo deliverable, §11 |
| **H3** | 64 KiB default chunk size — per-message `to_vec`, wakeups, framing ×16/MiB | **FIX LANDED** (256 KiB default; channel sized by 4 MiB byte budget) |
| **H4** | depth-2 rendezvous `sync_channel` between produce and sender | Addressed in R2 (§3.3 byte-sized decode→sender channel) |
| **H5** | SHA-256 without HW acceleration | Dev box confirmed `sha_ni` 2026-07-11; DL385 one-liner at leg 0 (§3.3) |
| **H6** | client-side write path (remfield-io buffering/fsync, client disk) | `client_write_rate` diag already landed (TIO-5b) isolates it |

H1+H2 compounding explains the field number; leg 0 (§10) re-runs the July
restore on main+R1 and pins the decomposed baseline the submitter is judged
against — the design still refuses to let read-ahead take credit for, or
blame from, a plumbing bug.

### 6.4 R1 record (LANDED main@5740f1a) and residuals

Shipped: watchdog-bounded `blocking_send` preserving per-chunk
deadline/error semantics (`send_with_timeout`, receiver-drop watchdog on an
OS thread — the panel's naive-`blocking_send` trap was avoided; diff-gated),
256 KiB default chunk, channel capacity from a 4 MiB byte budget
(`read_stream_channel_capacity`), explicit 4 MiB h2 stream+connection
windows, `sender_stall_ms` diag. **Dropped from v0.1's R1 list, per panel
(negative value):** item 4, zero-copy refcounted slices from
`StagedReadWriter` — sharing ring memory into the network path pins
reservoir buffers behind a slow client and re-couples exactly what the
reservoir decouples; the copy-out stays. Residuals: (a) the 30 s per-chunk
deadline is re-scoped by §4.5 when R2 lands (diagnostic tick + h2
keepalive, abort only on proven death); (b) client-side window/chunk
matching is a **sutra-agent deliverable** (§11); (c) field confirmation is
leg 0.

## 7. Throughput model (honest)

### 7.1 Anchors (all measured on this hardware)

| Anchor | Number | Source |
|---|---|---|
| st/dd read, same drive+HBA+cart | 246–293 MB/s | 07-08 morning battery |
| HH LTO-9 native ceiling | ~300 MB/s | drive spec (FH 400 does not apply) |
| Write submitter, 1 MiB grant, pre-TIO-5 cadence | 157–164 MiB/s @ 6.2–7.2 ms/cmd | 07-07b window |
| Wire+kernel cost, 1 MiB command | ~0.85–1 ms | dd vs cadence decomposition |
| July read, CLI-ish plumbing (pre-batch, ~256 KiB/cmd) | 82 MB/s | July window — consistent with sync ping-pong at ~3 ms/cmd |
| Field daemon restore | 13.28 MB/s | 07-07b — the §6 pathology, fix landed (R1) |

### 7.2 Chain budget at 300 MB/s target

| Link | Estimated capacity | Basis / caveat |
|---|---|---|
| Submitter feed (ioctl + ε gap) | ~700+ MB/s | write-side symmetric arithmetic; **read ioctl cadence never yet measured physically — the single biggest model uncertainty (§1.2); leg 0/1 measures it** |
| Tape/drive | ~300 MB/s | drive-limited by design (2×+ headroom above it) |
| Decode thread (copy-out + parse + SHA-256) | ~600 MB/s–1.5 GB/s | dev box `sha_ni` CONFIRMED; DL385 EPYC architecturally has it, one-line confirm at leg 0; 4 KiB-block tapes multiply per-record overhead ×64 — see below |
| Sender + tonic (R1 landed) | target ≥ 500 MB/s | leg 0 measures; the known pathology mechanisms are fixed |
| Network | localhost/10GbE ≥ 1 GB/s; **1GbE ≈ 110 MB/s** | deployment fact; 1GbE restores are network-bound — the reservoir (§4) turns that into parked batching, not shoe-shine |
| Client disk | tmpfs/NVMe ≥ 1 GB/s; SATA root ≈ 220 MB/s | TIO-4 measurement lineage |

**4 KiB-block geometry (panel, sharpened):** per-record fixed costs ×64 is a
property of the TAPE (AOX034 is initialized at 4 KiB), penalizing **every**
restore from that cartridge, not just small objects. Modeled ≈7.5% of a core
at 300 MB/s — likely fine, but it is a model. **Pre-freeze micro-bench**
(copy-out + parse loop at 4 KiB records, hermetic); records-coalesced
copy-out (memcpy whole handoff, parse in place) folds into R2 **iff the
bench shows it binding** (v0.1 Q6 answered).

**Multi-object reality (panel, convergent):** the table above is a
single-object transfer budget. Production clip-pull restores interleave
per-object positioning (SPACE/LOCATE + spawn/join) with transfer;
positioning amortization is a named follow-up arc (§3.5), and the
acceptance suite gains a multi-object leg (§10 leg 4) so the shipped claim
covers the real workload shape.

### 7.3 Estimates (min of chain, per deployment)

- **Server-local restore to tmpfs/NVMe:** 250–300 MB/s — drive-limited.
  Acceptance gate: **300 MB/s is the target (owner decision, §11); the gate
  is mechanism-proven** — sustained rate ≥ drive-limited ceiling minus
  measured overheads, `feed_gap_us` p95 ≤ 500 µs, `free_wait_us` ≈ 0 — per
  TIO-5 §8 discipline (prove the mechanism, then read the number the drive
  allows).
- **LAN 10GbE client, fast disk:** ≈ the same, minus relay tax — expect
  230–280 MB/s; honest unknown until leg 0 decomposes R1 in the field.
- **1GbE client:** ~105–110 MB/s, network-bound. **Deployment note, not a
  defect and not a reason to skip R2 (owner):** the drive-side behavior is
  now §4's — in-band speed-matching if ≥ the drive floor, clean park/resume
  batching if below; never continuous shoe-shine.
- **What this design does NOT claim:** that 300 MB/s end-to-end is reachable
  on any path whose slowest link is below 300; that the 13.28 number is
  explained until leg 0 measures the landed R1 in the field; that
  4 KiB-block objects reach the same rate as 256 KiB ones (micro-bench,
  above); that read cadence equals write cadence (leg 0/1).

## 8. Instrumentation

Extends TIO-5b's read diag (already per-command `gap_us`/`ioctl_us` in the
funnel, restore_total decomposition line) — full spec in §3.6; summary:

- Submitter: `free_wait_us`, `park_us`, derived `feed_gap_us` (= gap −
  deliberate waits; the acceptance signal), window intent marker at window
  open. **Per-command audit events retained; no coalescing (§3.6).**
- Reservoir: live occupancy gauge, park/resume cycle counter, current
  regime (streaming / speed-matched / parked), effective reservoir bytes +
  clamp reason at window open.
- Decode: busy vs recv-wait. Sender: busy vs stall (`sender_stall_ms`,
  landed) — now a re-arming diagnostic tick, not a kill (§4.5).
- restore_total: `bottleneck=` (max busy fraction); `relay_ms` becomes
  sender-busy; exclusive-subtraction relay estimate dropped.
- Cross-thread counters: atomics or post-join snapshot (§3.6).
- **Drishti/viveka slow-I/O signal — DUAL-SIDED (owner decision):** emit a
  "drive below streaming rate / reposition-rate" signal for **both** restore
  (read: park/resume rate, sub-band regime entry, reposition counts from
  LOG SENSE at session close + live park counter) **and** ingest (write:
  feed-rate below streaming band on the write submitter's existing diag).
  This is the "unmonitored slow share quietly wears the drive" guard — the
  alert, wired into the viveka policy config, is what turns a chronically
  slow deployment from silent wear into an ops ticket. The **write-side
  big-buffer analog** (sizing the append spool so slow ingest sources batch
  cleanly instead of dribbling) is a noted symmetric FOLLOW-UP — not R2
  scope, recorded here so it isn't lost.

## 9. Invariants and crash rows

1. **Exactly one SCSI data command in flight, ever** — model transport
   asserts no overlapping execute across the three-thread scope; the drive
   handle is reachable from exactly one thread by construction (§3.1/§3.3).
2. **Golden-fixture regression with ZERO designed deltas — precondition
   named (panel m5):** the READ CDB sequence (counts, order, trailing
   partial batch, tripwire RP cadence, timeout classes) for a canonical
   **all-GOOD, full-batch** restore is captured from main (post-TIO-5b +
   §4.4 hardening) and must be **byte-identical** under the pipeline. The
   byte-identical claim holds only on the all-GOOD path — residual/
   recovered/filemark outcomes make subsequent counts runtime-derived by
   design; those paths are covered by the §5.2 assertions instead
   (`requested ≤ remaining_after_decode`), not by fixture identity.
3. **One funnel:** every READ goes through `read_block_batch` via
   `read_buffer_handoff`; no parallel decode, no submitter-side sense
   interpretation, **no audit-motivated fork (§3.6)**. Funnel-hardening
   commits (§4.4) modify the one funnel for all callers. (Codex
   additive-bias rule: wrap, don't copy — this is the single-safety-funnel
   statement for the prompt set.)
4. **Typed-handoff exposure unchanged + tightened:** the only path from
   pool memory to consumer bytes is `ReadBufferHandoff` honoring
   `valid_bytes`, now cross-checked against `bytes_transferred` (§4.4);
   sentinel test re-run across multi-window reservoir-scale reuse.
5. **No CDB after invalidation:** after ILI/reset-UA/poison, zero data CDBs
   without explicit reposition + proof (funnel gate, asserted). After any
   park > `T_reproof`: no data CDB before a passing READ POSITION re-proof
   (§4.3).
6. **No delivery past unproven position beyond the bound, for hash-less
   reads** (§5.5): ranged windows release bytes only ≤ the last proven
   cursor.
7. **Plan-bounded motion:** total records issued == plan, asserted; no
   speculative record, ever.

Crash/kill rows (reads are non-mutating — the table is short and must stay
short):

| Kill point | Recovery rule |
|---|---|
| mid-ioctl READ | no data hazard; session lost; reopen distrusts position per readiness rules (unchanged) |
| after handoff push, before consumer drains | handoffs are process-local; nothing was promised to the client that gRPC didn't deliver; client sees stream reset |
| consumer dead, submitter mid-ioctl | submitter completes the in-flight command, push fails, stops; ≤ reservoir+1 over-read, all discarded |
| decode thread panic | scope unwinds per §3.2 drop order: delivery close → submitter stops → sender poisoned → client gets error status (precedence §5.6) |
| kill while drive PARKED (reservoir full, §4) | reservoir is process RAM (§4.7): all buffered records lost, zero data risk; drive was motionless; reopen re-proves position as always |
| kill during poison drain | free-channel buffers are process memory; pool accounting is process-local; nothing durable to reconcile |

## 10. Testing

Hermetic (model transport / chaos), symmetric to TIO-5 §9's write rows:

- one-in-flight assertion under read pipelining (three-thread scope);
- golden READ-CDB fixture, zero deltas on the all-GOOD path (§9.2),
  timeout classes recorded;
- **staged-intent cancel matrix** (§5.2): FILEMARK+residual, ILI after N /
  before any, reset-UA on READ and after a GOOD batch (incl. 29/07),
  recovered-full, recovered-with-VALID-residual (⇒ fail-closed, §4.4),
  deferred sense, transport error, tripwire mismatch, GOOD+tripwire-RP-fail
  — each asserting (a) no boundary-crossing or stale-count READ is issued
  (`requested ≤ remaining_after_decode`, including all-GOOD tail batches),
  (b) handoff withheld/delivered exactly per the §5.1 table, (c) cursor/mode
  invalidation state matches TIO-5b's existing tests;
- **funnel hardening rows (§4.4):** `records_read × block_size >
  bytes_transferred` ⇒ completion-unknown, on filemark and recovered paths;
  impossible-residual fail-closed; write-side symmetric check;
- **error precedence rows (§5.6):** submitter SCSI error + racing decode
  panic + sender stall ⇒ client status carries the SCSI root cause;
  genuine client disconnect ⇒ disconnect status; terminal status emitted
  exactly once;
- sentinel stale-tail across multi-window reservoir-scale reuse;
- consumer-death row: drop the delivery receiver mid-window ⇒ submitter
  issues no further READs (≤1 completes), poison protocol ordering
  asserted (close-before-join, §3.2 drop order), pool accounting balances;
  symmetric row for free-channel disconnect;
- **reservoir/controller rows (§4):** watermark park at high-water /
  resume at low-water (sub-band) vs per-command gating (in-band); regime
  switch hysteresis (no flapping at the floor boundary); **position
  re-proof after park** (pause > `T_reproof` ⇒ RP before next READ; RP
  mismatch ⇒ poison); degenerate config rejection (`high ≤ low`, reservoir
  < minimum pool); RAM clamp-and-warn (budget below configured);
  refuse-to-start (below minimum pool); mlock-failure clamp; growth-step
  budget re-check; slow-alive client parked indefinitely with NO abort;
  dead peer while parked ⇒ h2/receiver-drop teardown, drive never moves;
  shared-ceiling contention (two streams, second clamps);
- slow-consumer row: throttled decode ⇒ submitter blocks/parks (never
  spins, never drops), `free_wait_us`/`park_us` recorded, `feed_gap_us`
  clean, all bytes exact;
- free-channel capacity ≥ pool / non-blocking return / construction
  assertion (`allocated == free headroom == delivery headroom`, §3.2);
- plan-bounded read-ahead: total records issued == plan exactly, for full
  object, ranged read (incl. first-block offset), and trailing partial
  batch;
- `HandoffBlockSource` validation parity: byte/record mismatch,
  filemark-early, zero-record outcomes reproduce today's `refill` errors
  byte-for-byte; decode-side `remaining` slaving (Σ received ≠ Σ issued at
  close ⇒ fail-closed);
- **ranged-read rows (§5.5):** ranged restore rides the pipeline (one-path
  proof); deliver-ahead bound enforced (bytes past proven cursor withheld
  until proof lands); tightened proof cadence honored;
- **4 KiB micro-bench (pre-freeze):** copy-out + parse at 4 KiB records;
  coalesced copy-out folds into R2 iff it binds;
- chaos kill rows per §9 table (incl. kill-while-parked);
- CLI path: file-writer consumer through the same core (one-path proof);
- Scenario: extend the restore scenario's `covers` with
  `rem.tape.read_pipeline`; full `~/system` suite green from clean slate.

Physical (next MSL3040 window), in order:

0. **Decompose-first (R1 field confirmation):** re-run the July 4 GiB
   restore on main@5740f1a (R1 landed) with TIO-5b diag BEFORE R2 is
   enabled — confirm H1/H2/H3 fixed in the field, pin the baseline R2 is
   judged against, measure read ioctl cadence (§1.2), and run the DL385
   `sha_ni` one-liner (§3.3).
1. Daemon restore, server-local tmpfs sink: sustained rate at the
   drive-limited ceiling; `feed_gap_us` p95 ≤ 500 µs; `free_wait_us` ≈ 0
   (drive-limited proof). Target 300 MB/s (§11).
2. Chunk-size and h2-window sweep over one LAN client (H2/H3 residuals);
   requires the sutra-agent client-side matching (§11).
3. **Throttled-consumer soak + floor qualification:** cap the client at
   ~100 MB/s and then well below the floor (~30 MB/s), ≥ 30 min each;
   qualify the drive's real speed-matching floor
   (`read_drive_floor_mib_s`); verify in-band ⇒ continuous speed-matched
   streaming, zero parks; sub-band ⇒ clean park/resume cycles at the
   configured watermarks, reposition counts (library syslog + LOG SENSE)
   ≈ predicted cycles and **orders of magnitude below un-reservoired
   shoe-shine**; park re-proof RPs visible in diag; Drishti signal fires.
4. **Multi-object restore leg (panel):** N-clip pull across the tape;
   measure per-object positioning share; this leg prices the §3.5
   follow-up arc with data.
5. Restore + append dual-drive concurrent leg (joins TIO-5 §8 leg 2 — the
   HBA-decision leg); shared RAM ceiling behavior observed (§4.6).
6. 4 KiB-block object restore (AOX034 is already initialized at 4 KiB) —
   per-record cost reality check against the pre-freeze micro-bench.

## 11. Config and rollout

Config: §4.8 (two existing keys, four new reservoir/controller keys, all
documented in `reference-configuration.md`, degenerate configs rejected at
load). **No mode switches** (v0.7 one-path rule / NOT-in-production
policy): the pipelined read path is THE read path; backout is git revert +
previous binary; old behavior survives as the golden READ-CDB fixture and
the existing cross-version stored-image tests.

Stages (each independently landable, diff-gated, scenario-verified):

1. **R1 — relay fixes: LANDED** (main@5740f1a, 2026-07-11, diff-gated).
   Field confirmation = leg 0. R1 was also R2's *operational* prerequisite
   (panel): without it, R2 would have starved every restore at the relay
   and parked/shoe-shined the drive for nothing.
2. **R2 — read submitter + reservoir: GO, target 300 MB/s (owner
   decision).** The cost lens argued R2 should be conditional on leg-0
   measurement (R1+sync ceiling ≈ 210–250 might clear a 250 gate; 1GbE
   paths cap at 110 regardless). **Owner overruled: 250 is insufficient;
   the target is 300, and R2 proceeds regardless of leg-0's number.** The
   lens's 1GbE honesty is kept as a §7.3 deployment note, not a gate.
   Additionally, R2 now carries the anti-shoe-shine reservoir — a wear
   mechanism wanted independent of peak throughput — which moots
   gate-on-throughput on its own. Scope: §3 architecture (incl. retyped
   consumer), §4 reservoir/controller, §4.4 funnel hardening (prerequisite
   commit), §5 state machine incl. ranged unification, §8 diag, §10
   hermetic rows. Prerequisite ordering within R2: funnel hardening →
   trait split (`BlockRead`) → submitter/reservoir → ranged unification.
3. **Cross-repo deliverable (owner decision): sutra-agent client-side
   matching.** The client must set matching h2 stream/connection windows
   and the 256 KiB chunk request on its channel, or H2 re-appears
   client-side on any non-local link. A sutra-agent prompt/design follows
   (that repo's docs/); if the flag surface spans repos, single-source it
   as a shared contract (`contract-read-stream-tuning.md`) per the
   referenced-contracts rule. Server-side values are already recorded in
   `reference-configuration.md` (R1).
4. **Drishti/viveka wiring (owner decision):** the dual-sided
   below-streaming-rate / reposition-rate signal (§8) lands with R2's diag
   and is registered in the viveka policy config. Write-side spool-sizing
   analog: follow-up, noted in §8.
5. Physical validation per §10; leg 3 corrects `read_drive_floor_mib_s`
   and the watermark defaults from measurement; leg 4 prices the
   positioning-amortization arc.

## 12. Open questions — v0.1 answers recorded, v0.2 residuals

**Answered (owner, 2026-07-11):**

1. ~~Shoe-shine acceptance~~ — **REJECTED.** Anti-shoe-shine is core; the
   reservoir (§4) is the mechanism. The throughput lens's reframing (RAM
   buys reposition *frequency*, and the access pole makes slow restores
   common) stands vindicated in the owner's direction.
2. ~~SHA-256 placement~~ — **inline on the decode thread, confirmed**;
   verification is decode/format-layer property per §3.4; dev-box `sha_ni`
   confirmed, DL385 one-liner at leg 0.
3. ~~R2 gating~~ (was the cost lens's biggest ask) — **R2 is GO, target
   300 MB/s**, not gated on measurement (§11).
4. ~~Client-side flag ownership~~ — **sutra-agent deliverable**, prompt to
   follow (§11.3).
5. ~~Naming~~ — **TIO-6** (commits already use it).
6. ~~4 KiB coalesced copy-out~~ — **in R2 iff the pre-freeze micro-bench
   shows it binding** (§7.2, §10).

**Remaining open (tracked for the verify round / field legs):**

1. **Read ioctl cadence** (§1.2/§7.2): the model's biggest uncertainty;
   leg 0/1 measures. If materially worse than write cadence, §7 reflows —
   the design's *structure* is unaffected, its numbers are.
2. **Drive speed-matching floor** (`read_drive_floor_mib_s` default):
   conservative 100 MiB/s until leg 3 qualifies the real band edge for
   this drive.
3. **Watermark defaults** (90/25 on a 4 GiB default reservoir): sane on
   paper; leg 3's park-cycle counts confirm or adjust. Sizing guidance
   (bigger reservoir for slow-consumer deployments) is documentation, but
   whether the *default* should be larger on big-RAM hosts is a deployment
   judgment for after leg 3.
4. **Per-range digests in RAO** (§5.5): format-layer question — would
   retire the ranged deliver-ahead bound. Owned by the RAO format thread,
   not R2.
5. **Sender-thread necessity post-R1/R2** (v0.1 Q4): keep the split (it
   isolates network stalls from hash/parse and now feeds the reservoir
   drain); with the decode→sender channel byte-sized (§3.3), measure, and
   simplify later if the sender is provably pass-through.
6. **Parked-session occupancy policy** (§4.5): park-and-wait is the v1
   rule (owner); if the field produces parked-forever sessions holding
   drive reservations, an idle policy is a *future* design with its own
   review — explicitly NOT designed now (NOT-in-production rule: no
   speculative eviction machinery).
7. **Positioning-amortization arc** (§3.5): leg 4 prices it; likely the
   next TIO thread after R2 for clip-pull workloads.
