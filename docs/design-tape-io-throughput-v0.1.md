# Tape I/O throughput — multi-block SCSI I/O, boundary position proofs, staged overlap — Design v0.1

**Status:** draft — panel pending.
**Date:** 2026-07-07.
**Problem source:** 2026-07-07 physical MSL3040 benchmarks. Sustained write
75.0/76.3 MiB/s (incompressible AND compressible — host-capped), sustained
restore 82 MB/s, against half-height LTO-9 drives whose native streaming rate
is ~300 MB/s. Confirmed by `remanence_write_diag` phase decomposition and
source inspection. The drives, media, block size, and gRPC transport are all
vindicated; the ceiling is Remanence's serial one-block-per-command I/O loop.
**Related:** `fable-review-msl3040-fieldtest-2026-07-07.md` (Track A),
`layer5-multi-object-append-design-v0.1.md` (durable-boundary rules, which
this design must not weaken), `lto9-media-readiness-design-v0.1.md`.

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
  READ POSITION per block ("so the caller learns the LBA of the block it just
  wrote").
- `crates/remanence-api/src/pool_write.rs:1912` — diag hardcodes
  `drive_write_per_block_read_position = true` (it is not a knob).
- `crates/remanence-api/src/lib.rs:1975-2100` — `append_object` is
  store-and-forward: full spool file, then tape write; phases strictly serial.
- `crates/remanence-daemon/src/main.rs:76` — spool dir hardwired to
  `state_dir/spool`.
- Drive config already runs the drive in fixed-block mode:
  `drive_prepare` logs `prior_block_size: Fixed { size_bytes: 262144 }`.

## 2. Executive decision

Remanence adopts four levers, ordered by leverage:

1. **L1 — Fixed-mode multi-block WRITE/READ.** Issue `FIXED=1` WRITE(6)/READ(6)
   CDBs transferring N records per command (default batch 16 records = 4 MiB,
   configurable, capped by device/HBA max transfer). The on-tape format is
   **unchanged**: the tape carries the same fixed 256 KiB records either way.
   This divides SCSI round trips by N.
2. **L2 — Boundary position proofs instead of per-block READ POSITION.**
   Track the logical position arithmetically during a transfer; issue real
   READ POSITION only at transfer start, after the filemark, after ANY
   non-clean command outcome (CHECK CONDITION, EW, EOM, short transfer,
   transport error), and as a periodic drift tripwire (default every 1 GiB).
   Any mismatch between arithmetic and device-reported position is
   fail-closed: poison the session, fence the tape, keep the prior committed
   prefix authoritative.
3. **L3 — Overlap the transfer stages.** Double-buffer between the source
   (spool file read) and the tape sink inside the transfer loop so disk read
   and tape write proceed concurrently; same shape on the read path (tape
   read ∥ gRPC send). This is deliberately smaller than the full A7 streaming
   redesign (which eliminates the spool file entirely) — A7 remains a separate
   follow-up design; L3 is the low-risk overlap available inside the current
   architecture.
4. **L4 — Spool placement and failure honesty.** `daemon.spool_dir` becomes a
   config key (default unchanged: `state_dir/spool`); document that spool
   contents are pre-commit and crash-disposable, so tmpfs is a legitimate
   production choice. `create_private_spool_dir` must detect a dangling
   symlink and say so (the field failure surfaced as an opaque
   "File exists"). A failed spool create must reach the client as a real
   gRPC status describing the cause — never a bare stream close (field
   symptom: `append stream closed while sending Chunk` with the true cause
   invisible).

Non-goals of this design: A7 full streaming ingest, lazy dismount policy,
MR-9 live-status phases, close/unmount robotics overlap (§7 records the
measurements for the follow-up).

## 3. L1 — fixed-mode multi-block I/O

### 3.1 Semantics

SSC: with `FIXED=1`, TRANSFER LENGTH is a count of records of the block
length in the mode block descriptor. The drive is already configured
`Fixed { 262144 }` by `drive_prepare`, and every record Remanence writes is
exactly 262,144 bytes (`min_block_bytes == max_block_bytes == 262144` in
field diag). A batch WRITE(6) FIXED=1 with TRANSFER LENGTH=16 therefore
writes sixteen 256 KiB records — byte-identical on tape to sixteen serial
variable-mode writes.

- Batch size: `tape_io.write_batch_blocks` (default 16 = 4 MiB), clamped by
  the 24-bit TRANSFER LENGTH limit, the Block Limits VPD maximum transfer
  length, and the host `max_sectors` limit for the sg device. A final partial
  batch transfers the remaining records.
- Reads: batches never cross a tape-file boundary. The catalog/journal knows
  each tape file's exact `block_count`, so the reader sizes batches as
  `min(batch, remaining_records_in_file)`; a filemark encountered anyway
  (FM bit + residual) is handled defensively and fail-closed.
- `tape_io.write_batch_blocks = 1` is the safety valve: it must reproduce
  today's per-record behavior exactly (minus the per-block READ POSITION),
  preserving an escape hatch on hardware that misbehaves with batched I/O.

### 3.2 Partial-completion decoding (the safety core of L1)

A CHECK CONDITION mid-batch carries sense with VALID + INFORMATION =
**residual in records** (fixed mode). The handler must:

1. Decode `records_transferred = batch - residual` (bounds-checked; anything
   undecodable ⇒ completion-unknown, dirty fence, no arithmetic update).
2. For EW/EOM signals (`write_eom_signal` today): records_transferred counts
   as written; set early-warning state; then issue a REAL READ POSITION and
   require it to equal `position_before + records_transferred` — mismatch is
   fail-closed.
3. For any other CHECK CONDITION or transport error: no arithmetic update;
   READ POSITION; fence per the existing dirty-scope rules
   (`lto9-media-readiness-design-v0.1.md` §9 table).
4. ILI/short-transfer on reads: same decode discipline; a short read that
   isn't a known filemark boundary is fail-closed.

The durable-boundary rules from the layer-5 design are untouched: object
bytes → one synchronous filemark → READ POSITION proof → journal fsync →
SQLite. Only the number of CDBs between those boundaries changes.

### 3.3 Expected effect

Today's write cycle ≈ 3.33 ms/record (WRITE + READ POSITION + serial spool
read). After L1+L2, a 4 MiB batch costs one WRITE round trip; with L3
overlapping the source read, the tape path should approach drive-native
streaming (~300 MB/s on these HH drives) and the drive stops back-hitching
(feed rate rises above the ~100 MB/s speed-matching floor, which also
reduces mechanical wear — today's 75 MB/s feed keeps the drive shoe-shining).

## 4. L2 — position tracking and proofs

- `DriveHandle` keeps `expected_position: Option<u64>` per mounted session,
  updated arithmetically on clean outcomes (`+records`), invalidated on any
  ambiguity.
- `WriteOutcome.position_after` becomes the arithmetic position on clean
  batches; a new `position_proven: bool` distinguishes device-verified
  positions (boundaries, tripwires) from computed ones. Callers that require
  proof (filemark commit path, append-plan validation) demand
  `position_proven` — the layer-5 position-proof contract keeps device-read
  positions at its boundaries.
- Drift tripwire: every `tape_io.position_check_bytes` (default 1 GiB), one
  READ POSITION mid-stream; mismatch ⇒ poison session + fence tape + evidence
  record with both positions. This bounds undetected drift to one tripwire
  window while costing ~1 RP per 4096 records instead of one per record.
- Diag honesty: `position_calls` keeps counting real RPs;
  `drive_write_per_block_read_position` is replaced by computed fields
  (`write_batch_blocks`, `position_check_bytes`, `position_calls`).

## 5. L3 — staged overlap (bounded double-buffer)

Transfer loop becomes producer/consumer with two (configurable) in-flight
batch buffers:

- Write: spool-file reader thread fills buffers; the drive actor thread
  (existing blocking thread) drains them into batched WRITEs. Backpressure via
  the bounded channel; any sink error drains/poisons deterministically; the
  fail-closed outcome rules see exactly the same state as today.
- Read: drive actor produces batches; the gRPC sender consumes; client-side
  remfield-io already writes the out-file as chunks arrive.
- No change to commit ordering: the filemark is only written after every data
  batch has returned a clean outcome.

## 6. L4 — spool placement + error surfacing

- `daemon.spool_dir` config key; default `state_dir/spool`; INSTALL/runbook
  note the tmpfs option and its rationale (pre-commit data; loss = the write
  fails, never corruption).
- `create_private_spool_dir`: detect symlink targets (dangling → explicit
  error naming the symlink and target), create-through valid symlinks.
- `append_object` failures before/during spooling must map to a gRPC status
  with the underlying cause (`failed_precondition("spool create failed: …")`),
  and the daemon must log it at ERROR with the spool path. Field evidence
  showed three aborted sessions whose only client-visible symptom was
  "append stream closed while sending Chunk".
- (Pending fold: exact root cause of the ~60 s stream-close pattern from the
  2026-07-07 evidence — code trace in progress; the error-surfacing
  requirement stands regardless.)

## 7. Measured close/unmount overhead (recorded for follow-up, not fixed here)

Field numbers per session close: `actor_close_ms ≈ 107 s` after a 32 GiB
write (≈ 29 s after short/aborted sessions), `finish_mount_ms ≈ 30 s` robot
move-home, every time. (Pending fold: exact step decomposition from the code
trace.) Follow-up candidates, deliberately out of scope here: lazy dismount
(review doc A1), overlap of unload with client acknowledgment (the commit is
already durable at journal fsync), and immediate-mode filemarks at close
where safety allows. They interact with readiness fences and drive
stewardship and should ride the A1/A7 design, not this one.

## 8. Config surface

```toml
[tape_io]
write_batch_blocks = 16        # records per WRITE(6); 1 = legacy safety valve
read_batch_blocks = 16
position_check_bytes = "1GiB"  # drift tripwire cadence; 0 = boundaries only

[daemon]
spool_dir = "/path"            # optional; default <state_dir>/spool
```

## 9. Testing and coverage

Hermetic (model transport / chaos):

- CDB accounting: writing N records with batch B issues exactly ⌈N/B⌉ WRITEs,
  1 filemark, and ≤ (2 + N·record/position_check_bytes) READ POSITIONs;
  the same test at B=1 reproduces legacy counts minus per-block RPs.
- Fixed/variable equivalence: model transport asserts FIXED=1 batches carry
  TRANSFER LENGTH in records and the same payload bytes as B serial
  variable writes (tape image byte-identical).
- Partial-completion decode: injected CHECK CONDITION with residual R
  mid-batch → records accounting = B−R, mandatory RP, EW propagation;
  undecodable residual → completion-unknown fence.
- EW/EOM mid-batch → commit only after clean filemark + journal (crash table
  of layer-5 design §4.6 re-asserted under batching).
- Drift tripwire mismatch → session poisoned, tape fenced, evidence emitted.
- Read batching: never crosses tape-file boundary; injected FM-bit short read
  → fail-closed.
- Overlap (L3): source error mid-stream and sink error mid-stream both drain
  deterministically; no buffer is journaled/acked early.
- Spool: dangling-symlink spool dir → explicit error string; client receives
  cause-bearing gRPC status (not stream reset) — regression test at the API
  layer.
- Scenario: existing `~/system` suite (happy path, append, read, RAO archive)
  green from clean slate proves format compatibility end-to-end; extend
  `scenario-append`'s `covers` with `rem.tape.batched_io` once implemented.

Physical (next MSL3040 window):

- Rerun `20-bench-write.sh` / `21-bench-read.sh` with the new binaries;
  acceptance: ≥ 200 MB/s sustained write and read on the HH LTO-9 kit with
  tmpfs spool (target ~250–300); diag shows position_calls ≈ boundaries+tripwires.
- Rerun `13-append-loop.sh --mode session` and `--mode cycle`; per-object
  latency split recorded.

## 10. Rollout

1. **TIO-1** — L1+L2 in `remanence-library` (`write_block_batch`,
   `read_block_batch`, arithmetic position, boundary proofs) + model-transport
   tests. Callers still single-block.
2. **TIO-2** — pool_write/read-core switch to batched paths + diag fields +
   config keys + hermetic CDB-accounting coverage.
3. **TIO-3** — L3 overlap in the transfer loops (write + read).
4. **TIO-4** — L4 spool config + symlink-safe errors + append error
   surfacing + fieldtest/runbook updates.
5. Physical validation next window; then A7 streaming design builds on this.
