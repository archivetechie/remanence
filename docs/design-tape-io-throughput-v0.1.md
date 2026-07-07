# Tape I/O throughput — multi-block SCSI I/O, boundary position proofs, staged overlap — Design v0.2

**Status:** **FROZEN v0.2** — panel folded + verify round PASS (2026-07-07,
fresh codex: no new blockers/majors; 3 minors folded below — tmpfs policy
pinned to refusal, §9 units normalized, DevicePositionProof newtype named).
Prompt set: `prompt-tio-{1,2,3,4}.md`.
**Panel 2026-07-07:** 4 blind lenses (SCSI/SSC = Opus, failure-modes = codex,
cost/efficiency = Opus, general = GLM 5.2/OpenRouter): 2 blockers, 11 unique
majors, 9 minors, 4 nits; dispositions in
`panel-tape-io-throughput-2026-07-07.md`; all accepted findings folded here.
**Date:** 2026-07-07 (v0.1 same day).
**Problem source:** 2026-07-07 physical MSL3040 benchmarks. Sustained write
75.0/76.3 MiB/s (incompressible AND compressible — host-capped), sustained
restore 82 MB/s, against half-height LTO-9 drives whose native streaming rate
is ~300 MB/s. Confirmed by `remanence_write_diag` phase decomposition and
source inspection. The drives, media, block size, and gRPC transport are all
vindicated; the ceiling is Remanence's serial one-block-per-command I/O loop.
**Related:** `fable-review-msl3040-fieldtest-2026-07-07.md` (Track A),
`layer5-multi-object-append-design-v0.1.md` (durable-boundary rules, which
this design must not weaken), `lto9-media-readiness-design-v0.1.md`
(completion-unknown/dirty-scope rules, which §3.2 defers to).

---

## 1. Field measurements (2026-07-07, MSL3040, HH LTO-9 fw R3G3/S2S1)

Per 32 GiB object, `remanence_write_diag`:

| Phase | Time | Rate | Notes |
|---|---:|---:|---|
| mount_open | 2.9 s | | tape already loaded |
| spool (gRPC → spool file) | 146.7 s | 223 MiB/s | spool file on root disk |
| prepare (hash/plan) | 51.8 s | 633 MiB/s | |
| transfer (spool → tape) | 436.9 s | **75.0 MiB/s** | 131,075 × (read-256KiB-from-disk + WRITE(6) + READ POSITION), all serial |
| commit (journal fsync) | 3 ms | | excellent |
| close_unmount | 137 s | | actor close 107 s + robot move-home 30 s |

End-to-end wall per object ≈ 782 s ≈ **44 MB/s effective**. Restore of the
same object: **82 MB/s** (per-block serial read + gRPC relay + client file
write, serial; `read_block` issues NO per-block READ POSITION — proving the
serial round-trip loop, not the position read alone, is the dominant cost).
Compressible payload showed the same rate as incompressible: the drive was
never the limiter.

Code anchors:

- `crates/remanence-library/src/handle/tape_io/mod.rs:817` — `write_block`
  issues one variable-mode WRITE(6) per 256 KiB record, then an inline
  READ POSITION per block.
- `crates/remanence-api/src/pool_write.rs:1912` — diag hardcodes
  `drive_write_per_block_read_position = true` (it is not a knob).
- `crates/remanence-api/src/lib.rs:1975-2100` — `append_object` is
  store-and-forward: full spool file, then tape write; phases strictly serial.
- `crates/remanence-daemon/src/main.rs:76` — spool dir hardwired to
  `state_dir/spool`.
- The write-owner path already runs the drive in fixed-block mode
  (`drive_prepare` logs `prior_block_size: Fixed { size_bytes: 262144 }`);
  **the read path does not** (see §3.4 — panel blocker).

## 2. Executive decision and the honest ladder

Remanence adopts four levers. Panel-corrected framing: the transfer-phase win
is large (~4×), but **end-to-end per-object improves only ~1.6×**
(≈ 44 → ~72 MB/s on a 32 GiB object) until the spool and close phases are
also attacked — after this design lands, spool + prepare + close are >70% of
wall time. The per-lever throughput ladder for the transfer phase:

```text
75 MiB/s   today (serial, per-block RP, root-disk spool read in series)
~180       + L1/L2 (batched I/O, boundary proofs) — root-disk spool still serial
~223       + L3 (overlap) — now capped by root-disk spool READ rate
~286       + tmpfs/fast spool (L4 or the zero-code symlink) — drive-bound
```

Consequences the panel pinned: **spool placement is co-primary, not
tail-end** (with the drive in buffered mode and host feed below tape rate,
spool read rate IS the ceiling); L1+L2 alone likely land *under* the
≥200 MB/s acceptance gate on a root-disk spool, so the physical validation
run must have tmpfs spool in place; and the named next levers after this
design are **A7** (eliminate the spool file — streaming ingest) and **A1**
(lazy dismount — close/unmount is ~29% of post-fix wall), both in the field
review Track A, to be designed separately.

The levers:

1. **L1 — Fixed-mode multi-block WRITE/READ.** `FIXED=1` WRITE(6)/READ(6)
   CDBs transferring N records per command (default batch 16 records =
   4 MiB, configurable, clamped per §3.3). On-tape format **unchanged**.
2. **L2 — Boundary position proofs instead of per-block READ POSITION**,
   with a typed proof (§4) and fail-closed drift handling backed by a
   durable tape fence (§4.2).
3. **L3 — Overlap the transfer stages** (bounded double-buffer), with its
   own crash table (§5.1).
4. **L4 — Spool placement and failure honesty** (§6).

Non-goals: A7 streaming ingest, A1 lazy dismount, MR-9 live-status,
close-path IMMED=1 unload (all recorded in §7 with field numbers for their
own designs).

## 3. L1 — fixed-mode multi-block I/O

### 3.1 Semantics and interchange preconditions

SSC: with `FIXED=1`, TRANSFER LENGTH counts records of the mode block
descriptor's block length. The SSC TRANSFER LENGTH field is 24-bit (≈16M
records) — no CDB-format migration is needed. Variable-mode-written
262,144-byte records are byte-identical on tape to fixed-mode records and
interchangeable both ways, **contingent on two invariants that become stated
preconditions with assertions**:

- every rem record is exactly `block_size` bytes (true today: the final body
  block is zero-padded by `BodyBlockWriter::finish_after_tar_eof`, bootstrap
  blocks are full-size, and `read_core` already asserts `read == block_size`);
- the drive's configured fixed block length equals the tape's record length,
  sourced from the tape's own bootstrap/catalog row — never a constant.

Batch reads use `SILI=0` (the CDB builder's `FIXED=1` byte already encodes
it); suppressing ILI is forbidden — a size-mismatched record must fail loud
(ILI), not silently truncate.

Reads never intentionally cross a tape-file boundary: the reader sizes
batches as `min(batch, remaining_records_in_file)` from the catalog/journal
`block_count`. The **filemark backstop is load-bearing**: if the catalog
count is too large, the drive terminates the READ at the filemark with
FILEMARK + VALID + INFORMATION = residual records; the handler must decode
that residual to establish `records_read` and fail closed. Record counts on
reads come from that decode, never from byte counts.

### 3.2 Partial-completion decoding (the safety core of L1)

Two regimes, split per the readiness design's completion-unknown rules
(panel blocker):

**(a) Decodable outcomes — drive responsive.** CHECK CONDITION with
current-command sense (response code 0x70/0x72):

1. Fixed-mode `records_transferred` is computed from the **sense INFORMATION
   field** (units = records, `VALID` required, bounds-checked `0..=batch`;
   missing VALID or out-of-range ⇒ completion-unknown, regime (b)). Never
   derive record counts from the SG_IO byte residual.
2. For EW/EOM signals: the **post-event READ POSITION delta**
   (`position_after − position_before`) is the arbiter of records-on-tape;
   the residual is corroboration only. `position_before` is pinned to the
   exact pre-batch logical position.
3. **Deferred errors (response code 0x71/0x73) are completion-unknown,
   always** — under buffered mode they describe an *earlier* command's
   destage failure and must never be decoded as EW or a current residual.
   Existing helpers (`is_fixed_format`, `write_eom_signal`,
   `ili_signed_information`, `space_residual_if_early_stop`) accept 0x71
   today and must be tightened as part of TIO-1.
4. **Any partial object-data batch makes the object uncommittable**: if
   `records_transferred != requested`, or hard EOM, or undecodable residual —
   no object-closing filemark is written, no journal record is built, the
   session is poisoned, and a durable tape fence (§4.2) is persisted.

**(b) Transport-unknown — no follow-up data-path CDBs.** SG_IO transport
error, timeout, or reset evidence: persist the dirty fence per the readiness
design's dirty-scope table, mark position unknown, stop drive I/O for the
session. Position is re-proven only by a later recovery path that reopens
the drive through readiness admission. (v0.1 wrongly required an immediate
READ POSITION here.)

Durability framing (panel): per-batch GOOD status under buffered mode is
**not** a media-commit guarantee; durability is proven only by the blocking
WRITE FILEMARKS at the layer-5 boundary, whose ordering (object bytes →
synchronous filemark → position proof → journal fsync → SQLite) is untouched.

### 3.3 Batch sizing and the real transfer cap

The binding per-command cap is the **sg/HBA single-DMA limit**
(`max_sectors_kb`, sg reserved buffer), not Block Limits VPD (which reports
per-record byte limits and stays relevant only to the existing
`BlockTooLarge` check). TIO-1 must:

- set the sg reserved buffer (`SG_SET_RESERVED_SIZE`) to the configured
  batch size at drive open, query the achievable value, and clamp
  `write_batch_blocks`/`read_batch_blocks` to it;
- **log the effective batch size at drive open** — a silently clamped batch
  reads as "batching landed" while the win quietly evaporates;
- add a runbook/INSTALL step to raise and verify `max_sectors_kb` on the
  field host.

Default batch stays 16 records (4 MiB): anti-back-hitch protection comes
from the drive's ~1 GiB internal buffer plus average feed above the LTO-9
speed-matching floor (~100–112 MB/s), not from host batch size. The next
physical window sweeps batch ∈ {8, 16, 32, 64} in `20-bench-write.sh` to
locate the knee empirically before hard-coding anything larger.

### 3.4 Read-side mode setup (panel blocker)

`drive_prepare` (MODE SELECT to `Fixed{block_size}`) runs only on the
write-owner path today; the restore path opens the drive variable-mode. A
FIXED=1 READ against a zero block-length descriptor is an ILLEGAL REQUEST on
real drives. TIO-2 therefore adds a first-class read-side step: before batch
reads, MODE SELECT the drive to `Fixed{block_size_bytes}` **from the tape's
bootstrap/catalog row**, verify via MODE SENSE, and fall back to the legacy
variable-mode single-record path if the tape's block size cannot be
established. This also covers field tapes written at non-default block sizes.

## 4. L2 — position tracking and proofs

- `DriveHandle` keeps `expected_position: Option<u64>` per mounted session:
  `+records` per clean batch, **`+1` per filemark written**, invalidated on
  any ambiguity. Re-seed points (all carry device-proven positions today):
  transfer start, `SPACE(EOD)` before append positioning, `locate`, `space`,
  `rewind`, and after any EW/EOM READ POSITION.
- Position values are split into two distinct types (not a runtime flag):
  **`DevicePositionProof(lba)`**, constructible only from a real READ
  POSITION result, and **`ComputedPosition(lba)`** for arithmetic values.
  Every position-bearing outcome (`WriteOutcome`, `WriteFilemarksOutcome`,
  locate/space results) carries whichever it truly has; the layer-5
  append-commit constructor accepts **only `DevicePositionProof`**, so a
  computed position cannot reach the journal by construction. The
  post-filemark proof the layer-5 design requires stays a real device read.
- Drift tripwire: every `tape_io.position_check_bytes` (default 1 GiB), one
  READ POSITION mid-stream; mismatch ⇒ poison session + durable tape fence +
  evidence with both positions. A poisoned session invalidates the entire
  uncommitted prefix, so the tripwire window is forensic granularity, not a
  restore-correctness window. Note: under buffered mode the long-form READ
  POSITION reflects accepted (buffered) writes — the tripwire proves
  arithmetic/logical consistency, **not durability** (the blocking filemark
  does that).
- Diag honesty: `position_calls` counts real RPs; the hardcoded
  `drive_write_per_block_read_position` flag is replaced by computed fields
  (`write_batch_blocks`, `effective_batch_blocks`, `position_check_bytes`,
  `position_calls`).

### 4.2 Durable tape-I/O fence (panel major)

"Poison + fence" becomes admission control, not prose: extend the existing
quarantine store with a **tape-I/O scope** record keyed by tape UUID +
barcode (drift mismatch, partial-batch uncommittable, undecodable residual).
Enforced at pool selection, session open, and startup reconciliation;
released only via the existing operator quarantine-release flow. In-memory
session poisoning remains, but it is no longer the only mechanism.

## 5. L3 — staged overlap (bounded double-buffer)

Transfer loop becomes producer/consumer with two in-flight batch buffers
(depth 2 is sufficient — the drive's internal buffer is the shock absorber;
deeper host queues buy nothing while spool read is the ceiling):

- Write: spool-file reader thread fills buffers; the drive actor thread
  drains them into batched WRITEs. Backpressure via the bounded channel;
  any sink error drains/poisons deterministically.
- Read: drive actor produces batches; the gRPC sender consumes.
- No change to commit ordering: the filemark is only written after every
  data batch has returned a clean outcome.

### 5.1 L3 crash table (panel major)

| Kill point | Recovery rule |
|---|---|
| after producer read, before any batch write | no tape bytes; prior prefix authoritative; spool file deleted on restart |
| after a batch WRITE, before arithmetic cursor update | restart never trusts arithmetic state (in-memory only); tape has uncommitted tail → layer-5 fence rules apply |
| after tripwire mismatch, before durable fence persists | startup reconciliation of the open session + journal/SQLite prefix comparison re-detects; the object was never journaled, tail uncommitted → fence |
| after sink error with queued producer buffers | buffers are process-local; drain code path poisons session; on kill, same as uncommitted tail |
| after filemark + proof, before journal fsync | layer-5 crash table row unchanged: commit is journal-fsync-bounded; tail without journal record is fenced, never adopted |

Chaos coverage asserts each row (model transport + kill injection).

## 6. L4 — spool placement + error surfacing

- `daemon.spool_dir` config key; default `state_dir/spool`; INSTALL/runbook
  document the tmpfs option (pre-commit, crash-disposable data) — and that a
  fast spool is the **wear-safe** configuration, not merely fast: feed below
  the ~100–112 MB/s speed-matching floor keeps the drive back-hitching.
- **tmpfs RAM budget**: when `spool_dir` resolves to tmpfs, reconcile the
  spool budget (`SPOOL_MAX_BYTES`) against available RAM and **refuse** the
  append with a cause-bearing status beyond the RAM budget — a 64 GiB
  byte-budget on tmpfs can otherwise OOM the daemon under concurrent large
  objects. (v0.2 policy is refusal, fail-closed; an overflow-to-disk path is
  explicitly deferred — no such config key exists.) Loss remains
  write-failure-only, never corruption.
- `create_private_spool_dir`: detect symlinks; a dangling symlink produces
  an explicit error naming link and target (the field failure surfaced as an
  opaque "File exists"). Concurrent spool-file creation is already safe
  (UUID names + `create_new`) — now stated.
- `append_object` failures must map to a cause-bearing gRPC status **and**
  be logged daemon-side at WARN/ERROR with the spool path — today
  `lib.rs:1975-2099` has no tracing on any error return, so the real status
  exists only in the RPC trailer.
- Client-side error honesty: `remfield-io`'s stream-send helpers
  (`fieldtest/tools/remfield-io/src/main.rs:597-612`, siblings at 499/535)
  map any local channel close to a fixed string, discarding the cause; they
  must surface the RPC's terminal `Status`.
- Root cause of the 2026-07-07 triple failure (resolved): dangling spool
  symlink → instant `Spool::create` failure → handler error before the
  `phase=spool` diag line → client swallowed the cause and aborted → the
  observed ~57–60 s was the abort's close choreography (§7), not a stall.
  No 60 s constant exists in the codebase (verified); the spool budget was
  ruled out (untimed semaphore, 64 GiB cap).

## 7. Measured close/unmount overhead (recorded for the A1 design, not fixed here)

Field numbers per session close: `actor_close_ms ≈ 107 s` after a 32 GiB
write (≈ 29 s after short/aborted sessions), `finish_mount_ms ≈ 30 s` robot
move-home, every time. Post-TIO this is ~29% of per-object wall — A1 lazy
dismount is the clear next lever. Step decomposition (code trace 2026-07-07):

| Step | Where | Type | Contribution |
|---|---|---|---|
| session-close health snapshot (LOG SENSE tape alerts + error counters + SQLite) | `write_owner.rs:938-983`, invoked at `1960-1969`/`2020-2028` | serialized ahead of unload; non-gating by design, but decoupling it from the client-blocking close is the same semantics question as IMMED=1 unload — not a free win | small |
| SCSI LOAD/UNLOAD `0x1b` `IMMED=0` (drive flushes, rewinds from current position, ejects) | `handle/mod.rs:1451-1459`, `1502-1553` | rewind is physical and position-bound; the blocking wait is a design choice — an immediate+poll idiom exists for LOAD (`load_immediate`, `handle/mod.rs:1478-1486`) | dominant: ~100 s from 32 GiB deep vs ~25-29 s near BOT |
| changer MOVE MEDIUM drive→home slot | `mount.rs:622-654` | physical robot travel, correctly serialized after unload | ~30 s constant |

These ride the A1/A7 design; changing close semantics interacts with
readiness fences and drive ownership and is out of scope here.

## 8. Config surface

```toml
[tape_io]
legacy_single_block = false    # true = exact shipped behavior: variable-mode
                               # single-record WRITE/READ incl. per-block RP
                               # (the rollout/backout switch — panel major)
write_batch_blocks = 16        # records per WRITE(6); clamped to the sg/HBA
read_batch_blocks = 16         #   achievable DMA limit; effective value logged
position_check_bytes = "1GiB"  # drift tripwire cadence; 0 = boundaries only

[daemon]
spool_dir = "/path"            # optional; default <state_dir>/spool
spool_tmpfs_ram_budget = "…"   # required acknowledgment when spool_dir is tmpfs
```

`write_batch_blocks = 1` remains valid (single-record fixed-mode batches with
boundary proofs) but is **not** the backout switch; `legacy_single_block`
is.

## 9. Testing and coverage

Hermetic (model transport / chaos):

- CDB accounting: N records at batch B ⇒ exactly ⌈N/B⌉ WRITEs, 1 filemark,
  RPs = boundaries + tripwires only; `legacy_single_block=true` reproduces
  today's exact command stream (incl. per-block RP).
- Fixed/variable equivalence: FIXED=1 batches carry TRANSFER LENGTH in
  records and produce a byte-identical tape image to serial variable writes.
- Partial-completion decode: injected CC with INFORMATION residual R
  (VALID=1) mid-batch → accounting B−R + mandatory RP delta arbitration;
  VALID=0 or out-of-range residual → completion-unknown fence; **deferred
  0x71 sense → completion-unknown, never EW** (regression on the tightened
  helpers).
- Transport-unknown mid-batch → dirty fence, NO follow-up RP issued
  (assert absence), session stops.
- Partial batch ⇒ uncommittable: no filemark CDB, no journal record, durable
  tape fence row present; selection/open/startup all refuse the fenced tape
  until operator release.
- PositionProof type: append-commit construction from a Computed proof does
  not compile / is rejected; filemark outcome carries DeviceRead proof.
- Filemark position arithmetic: tracker +1 per filemark; boundary proof
  equality asserted.
- Read batching: batch never crosses a tape-file boundary; injected
  FILEMARK+VALID+residual short read decodes records_read and fails closed;
  SILI stays 0.
- Read-side mode setup: batch read against an unprepared (variable-mode)
  drive model is refused/re-prepared; block size sourced from tape row.
- L3 crash table: chaos kill at each §5.1 row.
- Cross-version compatibility: stored-image tests — old-code reads a
  new-batch-written image; new-code reads an old variable-written image.
- Spool: dangling-symlink dir → explicit error; client receives
  cause-bearing status (not a bare stream close); daemon logs the error
  path; tmpfs RAM-budget refusal.
- Scenario: existing `~/system` suite green from clean slate; extend
  `scenario-append` `covers` with `rem.tape.batched_io`.

Physical (next MSL3040 window), acceptance split by spool placement:

- root-disk spool: gate ≥200 MB/s (≈191 MiB/s) sustained write, expected
  ~235 MB/s (≈223 MiB/s, the spool read ceiling); tmpfs spool: target
  250–300 MB/s (≈238–286 MiB/s) write and ≥250 MB/s read; diag shows
  `effective_batch_blocks` = 16 (not silently clamped) and position_calls ≈
  boundaries + tripwires. All §9 targets are stated in MB/s (SI) with MiB/s
  conversions parenthesized.
- batch sweep {8,16,32,64} on `20-bench-write.sh`.
- `13-append-loop.sh` both modes; per-object latency split recorded.

## 10. Rollout

1. **TIO-1** — L1+L2 core in `remanence-library`: batched write/read
   primitives, sense-INFORMATION residual decode with deferred-error
   tightening, PositionProof, arithmetic tracker, sg reserved-buffer
   clamping + effective-batch logging, `legacy_single_block` path, model-
   transport tests.
2. **TIO-2** — pool_write/read-core switch to batched paths; **read-side
   MODE SELECT step (blocker fix)**; durable tape-I/O fence scope + admission
   enforcement; diag fields; config keys; hermetic coverage.
3. **TIO-3** — L3 overlap (write + read) + crash-table chaos coverage.
4. **TIO-4** — L4 spool config + tmpfs RAM budget + symlink-safe errors +
   append error surfacing (daemon logging + remfield-io status honesty) +
   fieldtest/runbook updates (max_sectors step, tmpfs guidance, batch sweep).
5. Physical validation next window (acceptance per §9); then the A7
   streaming-ingest and A1 lazy-dismount designs take over the remaining
   ~70% of end-to-end wall.
