# Tape I/O read pipeline (TIO-6) — read submitter, handoff ring, relay unblocking — Design v0.1

**Status:** **DRAFT v0.1** (2026-07-11) — for panel review. Not frozen; nothing
here is dispatch-ready. Naming provisional ("TIO-6"; "TIO-5c" also defensible —
panel/owner call, §12 Q5.
**Problem source:** the 2026-07-07b MSL3040 window measured field restore at
**13.28 MB/s** end-to-end (undecomposable at the time — read diag was missing),
while a different read path had done 82 MB/s on the same class of object and
the kernel `st` driver via `dd` sustained **246–293 MB/s on the same drive,
HBA, and cartridge**. TIO-5a/5b then landed the write hot-submitter and the
read-side SAFETY machinery (typed `valid_bytes` handoff, staged-CDB cancel,
fixed-ILI cursor invalidation, reset-UA parity, read diag decomposition) — but
reads remain **synchronous batched-refill-on-exhaustion**: the drive is idle
while the host parses/hashes/relays, and the host is idle while the drive
reads. This design is the throughput half: the read submitter, symmetric to
TIO-5 §3, targeting ~300 MB/s restore.
**Related:** `design-tape-io-pipelined-submission-v0.1.md` (frozen v0.7 — §3
is the template this mirrors, §5 is this design's charter, §6 the invariant
set), `report-st-harvest-2026-07-10.md` (F1/F2/F4 read semantics already
folded into TIO-5b; C4/O10 = why rem deliberately does NOT copy st's
kernel-buffer/backspace read-ahead), `memo-field-window-2026-07-07b.md`
(field evidence), `prompt-tio-5b.md` (LANDED main@d2618f7 — the safety code
this design wraps), `design-tape-io-throughput-v0.1.md` (TIO-1..4 error
machinery, untouched).

---

## 1. The gap, decomposed

### 1.1 What the read path does today (main @ d2618f7)

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
     (blocks when relay is behind)            ChannelWriter: chunk to 64 KiB
                                              to_vec per chunk
                                              try_send → tokio mpsc(16)
                                              on Full: sleep ≤10 ms, retry
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
  — plus a new one: the *network's* backpressure propagates directly into
  the SCSI submission gap.
- The relay thread re-chunks into 64 KiB protobuf messages (default when the
  client requests 0), copies again, and drives a `try_send` + **10 ms
  sleep** retry loop against a 16-deep tokio channel
  (`ChannelWriter::send_chunk`, `READ_SEND_RETRY_DELAY`).

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

### 1.3 Two distinct problems — do not conflate them

1. **The submission gap** (this design's core): host work between ioctls.
   Fixed by the read submitter (§3), the mirror of TIO-5 §3.
2. **The relay pathology** (§6): 13.28 vs 82 MB/s is a ~6× gap that no
   submission-gap model explains. Something downstream of the drive is
   grossly slow — hypotheses ranked in §6 with discriminators. **The
   submitter must land against the real bottleneck**, or the field number
   caps at wherever the relay caps, and the design gets blamed.

The 82 MB/s July figure is itself consistent with sync ping-pong (§7.1);
the 13.28 figure is the relay pathology stacked on top of it.

## 2. Approaches considered

**A — Hot read submitter on the drive actor, strictly one command in
flight, handoff ring to a decoupled consumer (ADOPTED).** Feed-rate
arithmetic transfers from the write side unchanged: ioctl ≈ 0.85–1 ms for
1 MiB when the drive buffer has data; inter-command gap ≤ 0.3 ms ⇒ drain
capacity ~740 MB/s ⇒ drive-limited (~300 MB/s HH LTO-9) with 2.5× headroom.
No command overlap needed, ever. st itself is this shape on reads (one
outstanding command; its read-ahead is host-side buffering, `st.c` C4).

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
documented-discarded this structure; this design re-affirms it.

**D — Fix the relay only, keep synchronous refill (REJECTED as
insufficient, but its relay fixes are ADOPTED as a first stage).** With the
relay unblocked and a fast consumer, sync refill's duty cycle is
ioctl/(ioctl + consume) ≈ 3.3/(3.3+0.7–1.5) ≈ 70–83% at 1 MiB/command ⇒
~210–250 MB/s ceiling, below target, and fragile: any consumer hiccup lands
directly in the submission gap. The relay fixes (§6.4) are necessary for
*any* approach and cheap; they ship first so the physical decomposition can
attribute the submitter's own contribution honestly (§11).

## 3. Architecture

Three moving parts; two of them already exist and are re-used.

```
drive-actor thread            decode thread                sender thread
(read submitter, §3.1)        (§3.3, new home for          (§3.3, exists today —
                              read_core's consumer          drain_staged_read_sender)
                              logic)
┌────────────────────┐        ┌───────────────────┐        ┌──────────────────┐
│ pop free buffer    │        │ recv handoff      │        │ recv bytes       │
│ clamp arithmetic   │        │ validate (as      │        │ chunk + send to  │
│ build CDB          │        │  refill does now) │        │ tonic (blocking, │
│ read_buffer_handoff│─hand──►│ format parse      │─bytes─►│  no sleep loop)  │
│  (THE 5b funnel)   │  off   │ SHA-256           │  chan  │                  │
│ push handoff       │  ring  │ return buffer ────┼──free──┼─► (to submitter) │
│ repeat             │        │                   │  chan  │                  │
└────────────────────┘        └───────────────────┘        └──────────────────┘
```

### 3.1 The read submitter (hot loop)

Runs on the drive-actor thread that owns the `DriveHandle` — no new
ownership or locking semantics, mirror of TIO-5 §3.2. GOOD-path loop, in
full: pop a free ring buffer (blocking only when the ring is exhausted,
§4) → recompute the clamp arithmetic
(`records = min(read_batch_blocks_effective, remaining_records_in_plan)`)
→ call **`BlockSource::read_buffer_handoff`** (which is `read_block_batch`
— the single READ funnel, timeout class, CDB build, sense decode, position
arithmetic, tripwire, diag histograms, all unmodified) → push the typed
`ReadBufferHandoff` into the delivery channel (never blocks, §3.2) →
decrement `remaining_records_in_plan` by `records_read` → repeat until the
plan is exhausted.

- **Plan-bounded read-ahead:** the total records to read are known before
  the window opens (`tape_file.block_count` from the catalog for full-object
  reads; `plan.block_count` for ranged reads). The submitter reads ahead of
  the *consumer*, never ahead of the *plan*. No speculative record is ever
  issued, so approach C's backspace machinery stays unnecessary by
  construction.
- **Staged-next as typed intent, not a kernel artifact.** Before each issue
  the submitter holds `StagedRead { buffer, records }` — host memory only.
  "The next READ's CDB may be staged" (TIO-5 §5) means exactly this; the CDB
  itself is rebuilt per command (`build_read_fixed_cdb(records)`, record
  count varies at the trailing partial batch — same rule as the write side's
  partial-batch CDB rebuild). Because the staged READ never reaches the
  kernel early (approach B rejected), **cancellation is control flow and is
  always possible**: see §5.2.
- **How read-ahead coexists with exactly one command in flight:** the
  submitter thread is *blocked inside* the SG_IO for the duration of each
  command — there is never a second command, staged or otherwise, at the
  transport. Read-ahead is achieved purely by what the submitter does NOT
  do between ioctls: no parse, no hash, no copy, no channel rendezvous with
  the network. On completion, the next buffer and record count are already
  in hand, so the next READ is issued within microseconds. The drive's own
  buffer, already filled ahead from tape, satisfies it immediately. Cadence
  ≈ ioctl + ε, exactly the write submitter's mechanism.
- Position seeding, the 1 GiB drift tripwire, and the 900 s TapeIo timeout
  class all live inside the funnel already
  (`seed_expected_position` is arithmetic when the cursor is cached;
  `advance_expected_position` fires the inline tripwire RP) — the submitter
  adds nothing and removes nothing.

### 3.2 Handoff ring and channels

Mirrors TIO-5 §3.1 with the buffer flow reversed (empty buffers flow to the
submitter; filled, typed handoffs flow away from it):

- **Ring:** `staging_ring_buffers` (existing key, default 4, validated
  2..=16; `BlockSource::read_ring_buffers` already plumbs it) buffers of
  `effective_read_batch_blocks × block_size` (1 MiB today: 4×256 KiB under
  the sg 1 MiB grant), checked allocation, effective ring bytes logged at
  window open (write-side parity). Read and write sessions are mutually
  exclusive per drive, so sharing the ring-depth key doubles no budget.
- **Free channel:** capacity ≥ ring size, pre-seeded with all buffers.
  Buffer return (from decode thread, `into_reusable_buffer`) is therefore
  always non-blocking — the write side's no-credit-loop-wedge rule.
- **Delivery channel:** bounded `sync_channel(ring)`. The submitter can hold
  at most `ring` buffers across in-flight + queued, so its push never blocks
  — asserted, not assumed (ring accounting test, §10). If the push fails
  (receiver dropped = consumer died), the submitter stops issuing
  immediately: **no tape motion for a dead client**, bounded by the one
  command already in flight.
- **Ownership handoff:** `ReadBufferHandoff` (TIO-5b's type) moves the
  buffer by value; `valid_bytes`/`records_read`/terminal flags travel with
  it. Fail-closed funnel outcomes return `Err` and never surface a buffer —
  unchanged. The stale-tail property (sentinel-prefill test) is unchanged
  because the exposure surface — the typed handoff — is unchanged.
- **Terminal poison protocol** (mirror of TIO-5 §3.1 / pool_write.rs
  1568-1588, roles reversed): on any error the submitter (a) stops issuing,
  (b) sends the classified error down the delivery channel exactly once (or
  poisons the shared flag if the channel is gone), (c) drains the free
  channel without issuing CDBs. Drop/join order: close delivery channel →
  decode/sender threads unwind → join. A decode thread parked in a blocking
  recv is always released by the channel close.

### 3.3 Consumer side: decode thread + sender thread (reuse, relocated)

The two-thread restore relay split already exists
(`stream_with_staged_read_sender_diagnostics`: produce closure + sender
thread). Today the produce closure runs on the drive-actor thread; the
change is **relocation, not invention**:

- The scoped-thread block grows to: drive-actor thread runs the submitter
  (§3.1); a spawned **decode thread** runs today's produce closure —
  `read_object_payload`'s format streaming, `CapturePayloadSink` (SHA-256),
  `StagedReadWriter` — against a **`HandoffBlockSource`** adapter; the
  existing **sender thread** (`drain_staged_read_sender` → `ChannelWriter`)
  is unchanged apart from §6.4's fixes.
- **`HandoffBlockSource`** is `BatchingBlockSource` with `refill()`'s SCSI
  call replaced by a channel `recv()`. Everything else in `refill` is
  consumer-side *validation* and stays byte-for-byte: the
  byte/record-mismatch check, the filemark-before-plan-end fail-closed
  error, `read_block`'s per-record copy-out. It implements only the read
  surface; `locate`/`space`/`position` are not available on it (compile-time
  removal — during a pipelined window the submitter owns all tape motion;
  see §5.4).
- The format parse, hashing, and copies now run **concurrent with the next
  SG_IO** instead of inside its gap. Consumer budget at 300 MB/s: one
  256 KiB copy-out per record + SHA-256 (~1.5–2 GB/s with SHA extensions;
  EPYC has them — measure, §8) + one `to_vec` per emitted slice ≈ 25–40% of
  one core (§7.2). If a future profile shows the decode thread itself
  binding, the split point between decode and sender can move — but that is
  a measurement-triggered follow-up, not v0.1 scope.

### 3.4 Session integration

`stream_one_object` / `stream_one_file_range` keep their signatures and
catalog work. `verify_loaded_tape_identity` (rewind + BOT bootstrap check)
and the SPACE to the tape file happen before the window opens, on the
actor thread, exactly as today — the pipelined window covers only the
record-transfer phase, mirroring the write side (position seeding and
motion never prep-ahead). CLI break-glass reads (`rem-debug archive read`)
route through the same core with a file-writer consumer; there is exactly
ONE read transfer path after this lands (v0.7 one-path rule — no
`read_pipeline` flag, no legacy refill mode kept; backout is git revert +
previous binary).

### 3.5 Diag restructure (decomposition under overlap)

TIO-5b's restore decomposition assumes serial phases
(`relay = wall − position − transfer`, `exclusive_restore_relay_phase`).
Under overlap that subtraction is meaningless — transfer and relay run
concurrently. The diag moves to per-thread busy/idle accounting:

- Submitter: existing `gap_us`/`ioctl_us` histograms (already recorded in
  the funnel) **plus new `free_wait_us`** — time blocked waiting for a free
  buffer. `free_wait_us` high ⇒ consumer-bound (the smoking gun for §4).
- Decode thread: busy time (parse+hash) vs recv-wait (drive-bound signal).
- Sender: busy vs channel-full-wait, plus a **stall counter/duration on the
  tonic channel** (replaces the information the 10 ms sleep loop silently
  destroyed).
- The restore_total diag line keeps its fields; `relay_ms` becomes
  sender-busy, and a new `bottleneck=` field names the thread with the
  highest busy fraction. Claim discipline unchanged from TIO-5 §5: the
  number becomes attributable; this design does not promise which link is
  slowest at any given site.

## 4. Backpressure and the slow-consumer truth (the crux)

**What the ring is for — and what it cannot do.** The ring absorbs
*scheduling jitter* (tens of milliseconds at 4×1 MiB) so the submitter never
blocks across ordinary consumer hiccups. The **drive's internal buffer**
(hundreds of MiB on LTO-9) absorbs *seconds* of downstream stall — the drive
keeps streaming from tape into its buffer while the host is slow. Nothing —
no ring depth, no host buffer — saves a consumer whose **sustained** rate is
below the drive's minimum streaming rate. Deeper rings only change the
stop/start *frequency*, not the average.

**What actually happens when the consumer is slower than the tape.** Free
channel empties → submitter blocks in `free_wait` → no READ issued → drive
buffer fills → drive stops tape motion. LTO speed matching first steps the
tape down through its discrete speed band (HH LTO-9 spans roughly full
native down to ~⅓ native; the exact floor for this drive is a qualification
item, §10 leg 3). Below the floor the drive enters stop/rewind/reposition
cycles — shoe-shine. Consequences: throughput degrades to the consumer's
rate (the stop/start overhead largely hides inside the wait), plus
mechanical wear on media and heads proportional to reposition count.

**Design decision: reads accept shoe-shine.** Named honestly: a restore to a
slow consumer (1GbE client ≈ 110 MB/s, a slow client disk, a stalled human
pipe) **will** run the drive below streaming rate and will shoe-shine at the
band floor. The design does NOT require the consumer to sustain the drive's
minimum rate, and does NOT add pacing machinery, because:

1. Reads carry **no data-integrity risk** under stop/start — the failure is
   wear and wall-time, not correctness. (Contrast writes, where the commit
   protocol shapes everything.)
2. The alternative — refusing or staging restores below a rate floor —
   costs either availability (refusal) or a disk-spool round trip
   (re-importing the write path's spool disease onto reads, plus capacity).
3. It is **observable**: `free_wait_us` + reposition counts (drive
   TapeAlert/LOG SENSE at session close) attribute it per session, so a
   chronically shoe-shining deployment becomes a visible ops fact with a
   deployment fix (faster client path, or accepting the wear).

The panel is explicitly asked to challenge this (§12 Q1) — it is the one
place this design trades hardware wear for simplicity, and it is an owner
call whether e.g. sustained 1GbE restores are common enough to matter.

**Not adopted:** consumer-rate admission checks; adaptive ring growth;
host-side mega-buffering (RAM = object size); restore-to-spool-then-serve.
Each is designable later without touching the submitter if the field says
so; the diag added here is exactly what that decision would need.

## 5. Safety composition — one funnel, wrapped, never duplicated

The submitter calls `read_buffer_handoff` → `read_block_batch` —
**unmodified**. Every TIO-5b behavior holds because the code that implements
it is the code that runs. What pipelining adds is *what the submitter does
with each classified outcome*:

### 5.1 Outcome table

| Funnel outcome (TIO-5b, unchanged) | Funnel already does | Submitter consequence (new) |
|---|---|---|
| GOOD, full batch | cursor advanced arithmetically; tripwire RP when due; diag recorded | push handoff; decrement plan; issue next |
| Current RECOVERED ERROR, no terminal flags (st F2) | success; `valid_bytes` = proven-complete records; audited-as-recovered | push handoff; decrement plan by **actual** `records_read`; staged intent recomputed (short outcome rule, §5.2); continue |
| FILEMARK + residual | `Ok` with `filemark=true`, `records_read` from sense INFORMATION; cursor advanced incl. the mark | **staged intent invalidated.** Plan not exhausted ⇒ truncated object ⇒ fail-closed error to consumer (exactly today's `refill` rule); no further CDBs. A filemark can never be crossed by a stale staged READ because the staged READ was never in the kernel |
| Fixed-mode ILI (st F4) | `mark_position_unknown()`; `Err`; handoff withheld | staged intent invalidated; poison protocol; **no further data CDB is even possible** — `ensure_data_command_state_valid` refuses until explicit reposition + device position proof |
| Reset-UA `06/29/xx` (st F1) | `invalidate_for_reset_unit_attention()` (cursor + cached mode validation); `Err` | same as ILI, plus mode re-verification required before any data command — enforced by the funnel's gate, not by submitter discipline |
| Deferred sense 0x71/0x73 | completion-unknown `Err` | poison protocol; stop |
| Transport error / undecodable | `Err` via `map_scsi` | poison protocol; stop |
| Tripwire RP mismatch | `mark_position_unknown()`; `Err` | poison protocol; stop |

### 5.2 The staged-READ-cancel interaction, made explicit

The tricky case the charter names: a pre-staged READ that must be cancelled
because the *prior* command's outcome (residual, filemark, ILI, error)
invalidates its parameters. Design:

- `StagedRead` is a typed state machine on the submitter:
  `Armed { buffer, records }` → consumed by issue, or
  `Invalidated { reason }`.
- **Invalidation rule:** any funnel return where
  `records_read != records_requested` **or** any terminal flag is set
  **or** `Err` ⇒ the staged intent is discarded *before* any issue, and
  `remaining_records_in_plan` / clamp are recomputed from post-decode state
  (`records_read`, cursor validity). Only a full-count, flag-free outcome
  (GOOD or recovered-full) lets an armed intent survive as-is.
- Because approach B was rejected, the staged READ exists only as host
  memory: cancellation cannot fail, cannot race the transport, and needs no
  kernel cooperation. This is TIO-5b's staged-CDB-cancel machinery promoted
  from a single-refill property to a loop invariant, with a hermetic
  assertion (§10): **no READ CDB is ever issued whose record count exceeds
  the post-decode remaining plan, under every non-full outcome in the
  matrix.**

### 5.3 New failure surfaces introduced by pipelining (owned honestly)

- **Consumer death mid-window** (client disconnect, decode panic): delivery
  push fails ⇒ submitter stops after the in-flight command; poison; drain;
  the drive-actor returns to its command loop. Bounded over-read: ≤ ring
  buffers already filled + 1 in flight — all discarded, no delivery. Test
  row, §10.
- **Submitter death / process kill:** reads are non-mutating; there is no
  fence, no journal, no commit exposure. Recovery is the ordinary readiness
  path (position distrust on reopen). TIO-5b's chaos row (drop-side never
  issues a destructor READ) extends to the full three-thread scope. Crash
  table, §9.
- **Poisoned-window buffer leak:** ring accounting (allocated == returned
  or dropped-with-window) asserted at window close, mirroring the write
  side's `RingAccounting` imbalance check.
- **Abort latency:** operator abort cannot interrupt a mid-ioctl submitter
  (known TIO-3/5 property, unchanged); with 900 s TapeIo timeouts a hung
  READ occupies the actor for up to 900 s. Pipelining neither worsens nor
  fixes this; stated for the record.

### 5.4 Motion fencing

During a pipelined window the submitter owns the drive exclusively (it IS
the drive-actor thread). `HandoffBlockSource` deliberately cannot express
`locate`/`space`/`position`, so consumer-side code cannot interleave motion
with in-flight reads even by accident. Ranged reads do their SPACE
(filemarks + blocks) before the window opens, as today.

## 6. The 13.28 MB/s pathology — hypothesis ladder

The 82→13.28 gap is ~6×; no submission-gap model explains it (sync refill
with a healthy relay predicts 70–83% duty, §2-D). Something downstream is
grossly slow. Ranked hypotheses, each with its discriminator in the
TIO-5b/§3.5 diag:

| # | Hypothesis | Mechanism | Predicted signature | Discriminator |
|---|---|---|---|---|
| **H1** | **`ChannelWriter`'s 10 ms sleep-quantized retry** (`read_core.rs::send_chunk`: `try_send` → on `Full`, `thread::sleep(≤10 ms)`) | with 64 KiB chunks and a 16-deep tokio channel, a consumer that drains bursty locks the writer into sleep quanta: 1–2 chunks per 10 ms ⇒ **6.5–13 MB/s** — brackets the field number | `client_write_ms` ≈ wall; sender busy-fraction low; stall counter huge | new sender stall counter (§3.5); or trivially: replace with `blocking_send` and re-run |
| **H2** | **h2 flow-control window at default 65,535 B** (no `initial_stream_window_size`/`initial_connection_window_size` anywhere in daemon or client tonic config — verified absent) | per-stream throughput ≈ window/RTT; fine on localhost, but ~13 MB/s at ~5 ms RTT (tunnel/VPN path) | throughput tracks RTT; localhost A/B differs by orders of magnitude | run the same restore localhost vs tunneled; capture SETTINGS frames |
| **H3** | **64 KiB default chunk size** (server default when the client requests 0) | 16 messages/MiB: per-message `to_vec`, wakeups, h2 framing, channel churn | sender busy-fraction high with small per-message bytes | chunk-size sweep 64 KiB → 1 MiB on the same object |
| **H4** | depth-2 rendezvous `sync_channel` + double copy between produce and sender | adds latency/copies; alone ≤ 2×, not 6× | decode-thread send-wait high | channel-depth sweep; secondary |
| **H5** | SHA-256 on the SCSI thread without HW acceleration | sha2 without SHA-NI ≈ 300–500 MB/s, shares the submission gap today | decode busy dominated by hash | one-off `openssl speed sha256` / perf on the DL385 EPYC |
| **H6** | client-side write path (remfield-io buffering/fsync per chunk, client disk) | server-blameless; `client_write_rate` already isolates it | `client_write_rate_mib_s` low while relay fast | TIO-5b diag, already landed |

H1 is the prime suspect (mechanism sufficient, magnitude matches, and it
compounds with H2 on any non-local link). **Decision folded into rollout
(§11): the relay fixes land and are field-decomposed BEFORE the submitter**,
so the submitter's acceptance number is measured against a healthy relay —
the design refuses to let read-ahead take credit for, or blame from, a
plumbing bug.

### 6.4 Relay fixes (stage R1 — small, independent, codex-sized)

1. Replace `try_send`+sleep with `blocking_send` (tokio mpsc supports it
   from sync context) + the stall-time counter. The 30 s liveness deadline
   survives as a watchdog around the blocking send, not as a poll quantum.
2. Raise the server default chunk size (client-requested 0) from 64 KiB to
   256 KiB–1 MiB (final value: measured, §12 Q4), and size the tokio channel
   in bytes, not messages.
3. Set explicit h2 windows on daemon and client (e.g. 4–8 MiB
   stream/connection), or enable tonic adaptive windows; record the choice
   in `reference-configuration.md`.
4. (Cheap, opportunistic) eliminate the `StagedReadWriter` `to_vec` by
   passing refcounted slices — only if it falls out naturally; copies are
   not the dominant term (§7.2).

## 7. Throughput model (honest)

### 7.1 Anchors (all measured on this hardware)

| Anchor | Number | Source |
|---|---|---|
| st/dd read, same drive+HBA+cart | 246–293 MB/s | 07-08 morning battery |
| HH LTO-9 native ceiling | ~300 MB/s | drive spec (FH 400 does not apply) |
| Write submitter, 1 MiB grant, pre-TIO-5 cadence | 157–164 MiB/s @ 6.2–7.2 ms/cmd | 07-07b window |
| Wire+kernel cost, 1 MiB command | ~0.85–1 ms | dd vs cadence decomposition |
| July read, CLI-ish plumbing (pre-batch, ~256 KiB/cmd) | 82 MB/s | July window — consistent with sync ping-pong at ~3 ms/cmd |
| Field daemon restore | 13.28 MB/s | 07-07b — the §6 pathology |

### 7.2 Chain budget at 300 MB/s target

| Link | Estimated capacity | Basis / caveat |
|---|---|---|
| Submitter feed (ioctl + ε gap) | ~700+ MB/s | write-side symmetric arithmetic; **read ioctl cadence never yet measured physically — the single biggest model uncertainty** |
| Tape/drive | ~300 MB/s | drive-limited by design (2×+ headroom above it) |
| Decode thread (copy-out + parse + SHA-256) | ~600 MB/s–1.5 GB/s | assumes SHA extensions (unverified on the DL385 — H5); 4 KiB-block tapes multiply per-record overhead ×64, needs its own measurement |
| Sender + tonic (post-R1 fixes) | target ≥ 500 MB/s | must be measured; today it is the pathology |
| Network | localhost/10GbE ≥ 1 GB/s; **1GbE ≈ 110 MB/s** | deployment fact; 1GbE restores will be network-bound and will stop/start the drive (§4) |
| Client disk | tmpfs/NVMe ≥ 1 GB/s; SATA root ≈ 220 MB/s | TIO-4 measurement lineage |

### 7.3 Estimates (min of chain, per deployment)

- **Server-local restore to tmpfs/NVMe:** 250–300 MB/s — drive-limited.
  Acceptance gate proposal: **≥ 250 MB/s sustained, and gap-histogram
  p95 ≤ 500 µs** (mechanism proven, not just the number, per TIO-5 §8
  discipline).
- **LAN 10GbE client, fast disk:** ≈ the same, minus relay tax — expect
  230–280 MB/s; honest unknown until R1 fixes are measured.
- **1GbE client:** ~105–110 MB/s, network-bound, drive below streaming rate
  (§4 applies). Not a defect; a fact to document in the runbook.
- **What this design does NOT claim:** that 300 MB/s end-to-end is reachable
  on any path whose slowest link is below 300; that the 13.28 number is
  explained until leg 0 (§10) measures it; that 4 KiB-block objects reach
  the same rate as 256 KiB ones (per-record costs ×64 — measure, then judge).

## 8. Instrumentation

Extends TIO-5b's read diag (already per-command `gap_us`/`ioctl_us` in the
funnel, restore_total decomposition line):

- Submitter: `free_wait_us` histogram (consumer-bound proof), window intent
  marker at window open (forensic parity with the write side: planned CDB
  count/bytes), coalesced TapeRead span per window (command count, bytes,
  duration summary) replacing per-command Started/Finished pairs on the good
  path — same coalescing rationale and same individually-audited exception
  list as TIO-5 §4 (errors, recovered, reset-UA, ILI, tripwire, filemark
  outcomes stay per-event with full sense).
- Decode: busy vs recv-wait durations.
- Sender: busy vs tonic-stall (counter + total duration) — the observable H1
  killed.
- restore_total gains `bottleneck=` (max busy fraction) and drops the
  exclusive-subtraction relay estimate (§3.5).
- All in-place counters/histograms, zero per-command allocation, read from
  shared diag state, never via a `DriveCommand` (actor busy for the window —
  unchanged known property).

## 9. Invariants and crash rows

1. **Exactly one SCSI data command in flight, ever** — model transport
   asserts no overlapping execute across the three-thread scope.
2. **Golden-fixture regression with ZERO designed deltas:** the READ CDB
   sequence (counts, order, trailing partial batch, tripwire RP cadence,
   timeout classes) for a canonical restore is captured from main
   (post-TIO-5b) and must be **byte-identical** under the pipeline — the
   clamp arithmetic, funnel, and plan are unchanged; only inter-command
   timing and thread placement change. This is a stronger property than the
   write side could claim (which had named deltas) and the panel should
   hold the implementation to it.
3. **One funnel:** every READ goes through `read_block_batch` via
   `read_buffer_handoff`; no parallel decode, no submitter-side sense
   interpretation. (Codex additive-bias rule: wrap, don't copy — this is
   the single-safety-funnel statement for the prompt set.)
4. **Typed-handoff exposure unchanged:** the only path from ring memory to
   consumer bytes is `ReadBufferHandoff` honoring `valid_bytes`; sentinel
   test re-run across multi-window buffer reuse.
5. **No CDB after invalidation:** after ILI/reset-UA/poison, zero data CDBs
   without explicit reposition + proof (funnel gate, asserted).

Crash/kill rows (reads are non-mutating — the table is short and must stay
short):

| Kill point | Recovery rule |
|---|---|
| mid-ioctl READ | no data hazard; session lost; reopen distrusts position per readiness rules (unchanged) |
| after handoff push, before consumer drains | handoffs are process-local; nothing was promised to the client that gRPC didn't deliver; client sees stream reset |
| consumer dead, submitter mid-ioctl | submitter completes the in-flight command, push fails, stops; ≤ ring+1 over-read, all discarded |
| decode thread panic | scope unwinds: delivery close → submitter stops → sender poisoned → client gets error status |
| kill during poison drain | free-channel buffers are process memory; ring accounting is process-local; nothing durable to reconcile |

## 10. Testing

Hermetic (model transport / chaos), symmetric to TIO-5 §9's write rows:

- one-in-flight assertion under read pipelining (three-thread scope);
- golden READ-CDB fixture, zero deltas (§9.2), timeout classes recorded;
- **staged-intent cancel matrix** (§5.2): FILEMARK+residual, ILI after N /
  before any, reset-UA on READ and after a GOOD batch, recovered-short,
  deferred sense, transport error, tripwire mismatch — each asserting (a)
  no boundary-crossing or stale-count READ is issued, (b) handoff
  withheld/delivered exactly per the §5.1 table, (c) cursor/mode
  invalidation state matches TIO-5b's existing tests;
- sentinel stale-tail across multi-window ring reuse;
- consumer-death row: drop the delivery receiver mid-window ⇒ submitter
  issues no further READs (≤1 completes), poison protocol ordering
  asserted (close-before-join), ring accounting balances;
- slow-consumer row: throttled decode ⇒ submitter blocks in free-wait
  (never spins, never drops), `free_wait_us` recorded, all bytes exact;
- free-channel capacity ≥ ring / non-blocking return assertions;
- plan-bounded read-ahead: total records issued == plan exactly, for full
  object, ranged read (incl. first-block offset), and trailing partial
  batch;
- `HandoffBlockSource` validation parity: byte/record mismatch,
  filemark-early, zero-record outcomes reproduce today's `refill` errors
  byte-for-byte;
- chaos kill rows per §9 table;
- CLI path: file-writer consumer through the same core (one-path proof);
- Scenario: extend the restore scenario's `covers` with
  `rem.tape.read_pipeline`; full `~/system` suite green from clean slate.

Physical (next MSL3040 window), in order:

0. **Decompose-first:** re-run the July 4 GiB restore on main+R1-fixes with
   TIO-5b diag BEFORE the submitter is enabled — confirm/refute H1–H6 and
   pin the baseline the submitter is judged against.
1. Daemon restore, server-local tmpfs sink: ≥ 250 MB/s sustained;
   `gap_us` p95 ≤ 500 µs; `free_wait_us` ≈ 0 (drive-limited proof).
2. Chunk-size and h2-window sweep over one LAN client (H2/H3 residuals).
3. **Throttled-consumer soak:** cap the client at ~100 MB/s for ≥ 30 min;
   observe speed-matching floor and stop/start behavior, capture reposition
   counts (library syslog + LOG SENSE), verify graceful degradation +
   correct `bottleneck=consumer` attribution. This leg prices the §4
   decision with data for the owner.
4. Restore + append dual-drive concurrent leg (joins TIO-5 §8 leg 2 — the
   HBA-decision leg).
5. 4 KiB-block object restore (AOX034 is already initialized at 4 KiB) —
   per-record cost reality check.

## 11. Config and rollout

```toml
[tape_io]
staging_ring_buffers = 4   # existing key, now read-side load-bearing too
read_batch_blocks   = 16   # existing key, unchanged semantics
```

No new config keys. **No mode switches** (v0.7 one-path rule /
NOT-in-production policy): the pipelined read path is THE read path;
backout is git revert + previous binary; old behavior survives as the
golden READ-CDB fixture and the existing cross-version stored-image tests.

Stages (each independently landable, diff-gated, scenario-verified):

1. **R1 — relay fixes** (§6.4): blocking send + stall counter, chunk-size
   default, h2 windows. Small, orthogonal, immediately field-measurable.
2. **R2 — read submitter**: §3 architecture, §5 state machine, §8 diag,
   §10 hermetic rows. Depends on nothing in R1 functionally, but its
   physical acceptance interpretation depends on R1 having landed (leg 0).
3. Physical validation per §10; results feed the §4 owner decision and the
   dual-drive/HBA thread.

## 12. Open questions (the owner / panel)

1. **Shoe-shine acceptance (owner, business-flavored):** §4 accepts drive
   stop/start under slow consumers, trading media/head wear for simplicity
   and availability. Consequence: routine restores over 1GbE or to slow
   clients run the drive below streaming rate. Recommendation: accept for
   v1 (wear is measurable, restores are rarer than ingests, and leg 3
   prices it); revisit only if reposition counts in the field say
   otherwise. Alternative if rejected: restore-to-server-spool for
   below-rate consumers, at disk and latency cost.
2. **Where SHA-256 verification lives:** inline on the decode thread
   (current behavior, keeps end-to-end hash-at-restore) vs deferred to a
   separate verify pass. Recommendation: inline — budget holds on paper
   (H5 measurement will confirm), and silent-restore-without-verify is the
   wrong default for an archive.
3. **Chunk-size + h2 window defaults (R1):** propose 256 KiB chunks /
   4 MiB windows as the measured-until-proven-otherwise starting point;
   the leg-2 sweep finalizes. Who owns the client-side flag surface
   (remfield-io / sutradhara seam)?
4. **Sender thread necessity post-R1:** with blocking_send and big chunks,
   does the decode→sender split still pay its complexity, or should decode
   write to tonic directly (two threads total)? Recommendation: keep the
   split in v0.1 (it exists, it isolates network stalls from hash/parse),
   measure, simplify later if the sender is provably pass-through.
5. **Naming:** TIO-6 vs TIO-5c. Cosmetic; INDEX row exists either way.
6. **4 KiB-block objects:** if leg 5 shows the decode thread binding at
   4 KiB records, is a records-coalesced copy-out (memcpy whole handoff,
   parse in place) in scope for R2, or a follow-up?
7. **Read cadence assumption:** the ~1 ms/1 MiB READ ioctl figure is
   write-side-derived; if leg 0/1 measures materially worse (drive read
   buffer behavior differs), the §7 model reflows — flagging so the panel
   treats §7 as a model, not a promise.
