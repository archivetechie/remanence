# Remanence Implementation Testing Plan

**Status:** v0.1 — cross-layer test gate for starting implementation
of Layer 3c v0.4.4, rem-tar v0.9.3, and the 3b catalog follow-up.

This document is intentionally stricter than the implementation-plan
snippets inside the design docs. The design docs say *what* to build;
this file says how hard the implementation must be attacked before it
is allowed near production tape.

The test sequence is:

```text
pure logic → mock tape → catalog transactions → QuadStor → scratch LTO
```

No production cartridge should be written until the relevant gates below
are green on mock tape and then on QuadStor.

---

## 1. Test principles

1. **Mock first.** Every rule that can be tested without hardware must
   be tested without hardware. Live tape is for confirming SCSI/drive
   behavior, not discovering basic format bugs.
2. **Corrupt deliberately.** A parity layer that only passes clean
   round-trips has not been tested. Every codec, map, and reader must
   be exercised with damaged blocks, wrong CRCs, missing filemarks,
   wrong block sizes, damaged sidecars, and stale catalog rows.
3. **Crash windows are first-class.** For every catalog commit point,
   test crash-before-filemark, crash-after-filemark-before-DB, and
   crash-after-DB.
4. **Standard tools are part of the spec.** GNU tar, bsdtar, and Python
   `tarfile` compatibility are release gates for uncompressed rem-tar
   objects.
5. **No hidden trust.** If a test passes because the catalog happens to
   be present, rerun the equivalent catalog-less path. If a test passes
   because the final bootstrap is intact, rerun with only an intermediate
   bootstrap.
6. **Every invariant gets an assertion.** The mock tape should make it
   impossible to accidentally write a short fixed block, write a body
   filemark, exceed `projected_size_blocks`, commit before a synchronous
   filemark, or treat an unvalidated suffix as trusted.

---

## 2. Pure codec and math gates

### 2.1 CRC-64/XZ

Required tests:

- Check vector: `CRC64_XZ("123456789")` equals the value in the 3c
  spec.
- Empty buffer, one-byte buffer, all-zero block, all-0xff block, and
  random 256 KiB blocks.
- Endianness: packed CRC bytes in sidecar/rem-tar structures must match
  the spec exactly.
- Mutate one bit in a 256 KiB block and assert the CRC changes.

### 2.2 RS / GF(2^8) implementation

Required tests:

- Appendix A encode vector.
- Appendix A reconstruction vector.
- Randomized encode/reconstruct for small schemes such as `(k=2,m=2)`,
  `(k=4,m=2)`, `(k=8,m=3)`.
- Default-scheme smoke at full block size using deterministic pseudo-
  random data.
- Incremental accumulation must equal batch encode byte-for-byte.
- Implicit-zero final-epoch shards must produce the same parity as
  explicit zero blocks.
- If `reed-solomon-erasure` is used, crate output must match the
  normative appendix vectors and randomized cases. If it does not, the
  spec wins.

### 2.3 Ordinal and stripe mapping

Property tests:

- `ordinal_to_stripe` and `stripe_data_to_ordinal` round-trip every data
  ordinal in small synthetic epochs.
- Consecutive ordinals distribute across stripes according to the
  row-major interleave.
- Every `(stripe_index, data_index)` appears exactly once per epoch.
- Final partial epoch with `D ∈ {0,1,k-1,k,k+1,S*k-1}` maps correctly.

---

## 3. Sidecar and bootstrap wire-format gates

### 3.1 Sidecar codec

Required tests:

- Exact byte offsets for block 0 fields.
- `block0_crc64` covers header and inline index bytes.
- Spill index blocks have trailing CRCs and zero-filled unused bytes.
- Parity index entries are 16 bytes and data CRC entries are 8 bytes;
  the writer never splits an entry across inline/spill boundaries.
- Sidecar with default geometry has the expected header/index block
  count.
- Corrupt one bit in block 0 → parse fails.
- Corrupt one index block CRC → parse fails.
- Corrupt one parity shard → parity-shard CRC fails and recovery treats
  it as an erasure.
- Corrupt one data peer that reads clean → data CRC fails and recovery
  treats it as an erasure.

### 3.2 Bootstrap codec

Required tests:

- Exact byte-offset table.
- `cbor_payload_len` is covered by the header CRC.
- Payload CRC covers exactly the payload bytes.
- Wrong magic, wrong schema, wrong tape UUID, corrupt payload length,
  corrupt header CRC, corrupt payload CRC all fail distinctly.
- BOT bootstrap plus intermediate bootstrap plus final bootstrap:
  discovery uses first valid for scheme and highest-scope for map
  validation.
- Candidate block-size fallback: valid 256 KiB bootstrap is found when
  the first candidate is wrong; wrong configured size fails cleanly.
- A short fixed-block read during bootstrap discovery is a hard error,
  not a partial parse.

---

## 4. Filemark map and digest gates

Required tests:

- Canonical projection digest is stable across map insertion order.
- Digest excludes content hashes and bootstrap payload bytes; final
  bootstrap can include its own structural map entry without circularity.
- Full-map bootstrap validates a complete reconstructed map.
- Intermediate bootstrap validates only its prefix.
- Prefix scope exposes only `TarOnlyUnverified` access outside the
  validated prefix; parity recovery outside prefix returns
  `OutsideValidatedMapPrefix`.
- `map_total_data_ordinals` and `highest_protected_ordinal` differ in
  an open-epoch case; recovery uses the latter.
- Damaged sidecar header with no catalog yields
  `FilemarkMapDigestMismatch`; tar-only extraction still works.

---

## 5. Mock raw tape gates

Implement a deterministic in-memory tape:

```text
Vec<TapeFile>
TapeFile = Vec<FixedBlock> + trailing filemark state
```

It must model:

- fixed block size;
- filemarks;
- EOD;
- early warning;
- EOM;
- synchronous vs failed filemark writes;
- physical position hints;
- append-truncates-after-current-filemark behavior;
- power-loss snapshots.

Required tests:

- Body formats cannot write filemarks.
- Sidecars and bootstraps bypass `ParityDataOrdinal`.
- Object blocks receive ordinals and update parity accumulators.
- `begin_object` rejects writes outside an active object.
- `finish_object` refuses to run if `BodyBlockWriter` has not flushed a
  whole final block.
- Exceeding `projected_size_blocks` is an invariant violation.

---

## 6. rem-tar layout gates

### 6.1 Pax-padding solver

Required tests:

- The solver equation is:

```text
O + 512 + roundup512(P) + 512 ≡ 0 mod chunk_size
```

- Exercise pax record length digit boundaries: 9→10, 99→100,
  999→1000, 9999→10000.
- Random path lengths, xattr lengths, caller object IDs, and pad lengths.
- Every regular file's first data byte is exactly at BodyLba boundary.
- No standalone padding pax member is ever emitted.
- `REMANENCE.pad` appears only inside the real pax header attached to
  the following entry.
- The global pax body is counted in `projected_size_blocks`.
- Counting-mode projection is always `>=` actual emitted blocks and
  should equal actual for deterministic fixtures.

### 6.2 BodyBlockWriter

Required tests:

- Never emits a short block mid-object.
- Final partial block is zero-filled only after tar EOF.
- File data tail is not zero-padded inside the tar payload.
- A block can contain file tail + tar 512 padding + the next pax header.
- Reported `first_chunk_lba` equals the block where file data starts.
- Empty files have `chunk_count=0` and no `first_chunk_lba`.

### 6.3 Standard tar compatibility

For each fixture, extract with GNU tar, bsdtar, and Python `tarfile`:

- one empty file;
- one 1-byte file;
- one file ending exactly at chunk boundary;
- one file ending one byte before chunk boundary;
- one file ending one byte after chunk boundary;
- many small files;
- one large sparse test file written as full bytes;
- symlinks including dangling external symlinks;
- hardlinks;
- directories with metadata;
- manifest sidecar extraction.

Assertions:

- extracted file bytes are exact;
- no trailing zeros are added;
- tar does not stop early at an accidental EOF;
- symlink target strings are exact and no target resolution is required;
- archival/minimal ustar ownership/mode sanitization behaves as specified.

---

## 7. rem-tar read and verification gates

Required tests:

- Tier 0 full-object read restores all entries.
- Tier 1 single-file read uses catalog `(tape_file_number, first_chunk_lba)`.
- Tier 2 byte-range read covers single-chunk, multi-chunk, head/tail trim,
  empty range rejection, and out-of-bounds rejection.
- Manifest direct read uses `manifest_size_bytes` and
  `manifest_chunk_count`; it does not seek backward to the manifest pax
  header for size.
- Manifest SHA mismatch refuses to trust per-chunk CRCs.
- Chunk CRC mismatch triggers `ObjectParitySource::recover_block_at`.
- Successful forced recovery suppresses the read error and logs audit.
- Failed forced recovery returns unrecoverable and falls back to another
  copy.
- File SHA mismatch after all chunk CRCs pass is surfaced as a hard
  integrity failure.

---

## 8. Layer 3c recovery gates

Required corruption matrix:

- single object-data block medium error;
- multiple object-data erasures within one stripe, `n <= m`;
- `m+1` erasures in one stripe → unrecoverable;
- contiguous damage lengths: 1, 50, S-1, S, 2S, mS, mS+1;
- damage spanning an object filemark;
- damage spanning object + sidecar cluster;
- parity shard damage only;
- data peer silently corrupt but clean-reading;
- parity peer silently corrupt but clean-reading;
- failed block below watermark in a partial object → recover;
- failed block above watermark in the same object → pending epoch,
  unrecoverable on this tape;
- prefix-map object outside validated prefix → no parity recovery;
- destroyed sidecar header → catalog-less recovery unavailable;
- catalog-present recovery despite damaged scan path.

Every successful reconstruction must verify the reconstructed data block
against the sidecar data CRC before returning bytes.

---

## 9. Capacity, EOM, and spool gates

Required tests:

- `begin_object` reserve includes object blocks, object filemark,
  pending sidecars, sidecars completed by the projected object, final
  partial sidecar, remaining bootstraps, and safety margin.
- Tape-capacity shortfall returns
  `CapacityReserveExceeded { cause: TapeCapacity }` before any object
  block is written.
- Spool-capacity shortfall returns
  `CapacityReserveExceeded { cause: ParitySpoolCapacity }` before any
  object block is written.
- Remedies differ: tape-capacity closes the tape; spool-capacity does
  not switch tapes.
- Projected object larger than empty-tape usable capacity is rejected;
  no mid-object tape spanning.
- EW/EOM during object data, object filemark, sidecar, and bootstrap
  follow the normative table in 3c.
- Sidecar clustering after a huge object does not overflow spool.

---

## 10. Crash/restart/append gates

For each scenario, run against mock tape, then QuadStor, then one scratch
LTO tape where physically safe:

1. Crash after object data but before object filemark: no catalog commit;
   partial object is abandoned.
2. Crash after object filemark before DB commit: tape has an extra file;
   catalog prefix wins; append truncates from last committed file.
3. Crash after DB commit for object: catalog and tape agree; append after
   that object.
4. Crash after object filemark before sidecar cluster: resume rebuilds
   the open epoch and continues.
5. Crash after sidecar 1 in a sidecar cluster: append point is after
   sidecar 1, not after the object.
6. Crash mid-sidecar: provisional sidecar map entry is discarded;
   append truncates before it.
7. Crash after sync filemark but before sidecar DB commit: tape has an
   extra sidecar; catalog prefix wins.
8. Crash after sidecar DB commit: sidecar is committed; watermark and
   object parity states are updated.
9. `W < T` open-epoch rebuild: reread `[W,T)`, re-accumulate, emit any
   now-complete sidecars, and load the partial epoch as live state.
10. Unreadable block during open-epoch rebuild: rebuild fails cleanly and
    Layer 5 falls back to another copy.
11. Resume-generated sidecars are committed with the ordinary sidecar
    catalog transaction and included in `ResumeAppendResult`.
12. Deliberate power loss after synchronous filemark confirms catalog
    commit ordering on real hardware.

---

## 11. Catalog transaction gates

Required tests against the in-process SQLite catalog projection, plus the
`no_hosted_database` dependency gate. Hosted databases such as Postgres are
outside Remanence; an external orchestrator may mirror state elsewhere, but the
authoritative local catalog path is SQLite plus the audit/journal rebuild inputs.

- object row and `catalog_tape_files(kind='object')` row insert in one
  deferrable transaction;
- sidecar row insert advances `highest_protected_ordinal` in the same
  transaction;
- resume-generated sidecars use the same transaction path;
- object `parity_state` updates from pending → partial → protected as
  watermark advances;
- `catalog_objects.data_block_count == catalog_tape_files.block_count`;
- unique `(tape_id, epoch_id)` for sidecars;
- hardlink target FK is deferrable and correct;
- `compression = 'none'` constraint holds for v1;
- staged migrations work on populated catalogs: add nullable, backfill,
  then set NOT NULL;
- crash after tape filemark before DB commit leaves tape/cat mismatch that
  restart resolves by catalog prefix;
- crash after DB commit leaves tape/cat agreement.

---

## 12. QuadStor gate

Before live LTO:

- Record the operator-discovered QuadStor topology before running the
  destructive parity suite:

```text
REM_QUADSTOR_PARITY_DRIVE_PATH=/dev/sgN
REM_QUADSTOR_PARITY_LIBRARY_SERIAL=<library serial>
REM_QUADSTOR_PARITY_DRIVE_BAY=0xNNNN
REM_QUADSTOR_PARITY_WRITE_LOOP=1
REM_QUADSTOR_PARITY_JOURNAL_PATH=/var/lib/rem/journals/quadstor-test.remjournal
```

  The 2026-05-25 live run used `/dev/sg0`, library serial
  `7CBAD9CF74`, drive bay `0x0100`, and a scratch cartridge loaded
  from slot `0x0400`. Future runs should rediscover these values
  instead of assuming that topology.

- write multi-object tape with several epochs and sidecar clusters;
- run the journal-wired parity session (`quadstor_parity_journaled_session`):
  `ParitySink::new_with_journal`, one object, checkpoint, reopen
  `FileTapeFileJournal`, and `plan_resume_append_from_journal`;
- reload and discover bootstrap + filemark map;
- run Tier 0/1/2 rem-tar reads;
- inject mock medium errors through the raw adapter if possible;
- run crash-window tests using forced process kill between mocked commit
  steps;
- run catalog-less scan reconstruction;
- run prefix-bootstrap-only recovery;
- run damaged-sidecar-header behavior;
- verify append-after-restart writes from the catalog prefix.

---

## 13. Live scratch-tape gate

Use one clearly labeled scratch LTO cartridge. Do not use production
media.

Required live tests:

- configure fixed block size and compression-off;
- write BOT bootstrap, object, sidecar, final bootstrap;
- unload/reload and discover bootstrap;
- `mt fsf N; tar -b 512 -xf /dev/nstX` extracts an object;
- full rem-tar Tier 0/1/2 read;
- append after clean close;
- deliberate daemon crash after object filemark;
- deliberate daemon crash after sidecar filemark;
- deliberate process kill after synchronous filemark before DB commit;
- one controlled power-loss/restart drill if hardware policy permits;
- confirm `write_filemark` path uses the synchronous barrier, not the
  no-flush immediate variant.

Live tests must record drive sense data, READ POSITION before/after each
commit boundary, and catalog rows before/after restart.

---

## 14. Release gates

A release may move from mock implementation to QuadStor only when:

```text
RS vectors green
sidecar/bootstrap codecs green
filemark-map digest green
rem-tar tar compatibility green
mock corruption matrix green
mock crash-window matrix green
catalog transaction tests green
```

A release may move from QuadStor to live scratch tape only when:

```text
QuadStor clean write/read green
QuadStor restart/append green
QuadStor catalog-less map recovery green
all DB migrations green on populated test catalog
```

A release may move from scratch tape to production only when:

```text
scratch tape tar extraction green
scratch tape append/restart green
synchronous filemark barrier verified
operator runbook written
fallback-to-other-copy path tested
```

---

## 15. Test artifact retention

For every failed gate retain:

- test seed;
- tape geometry;
- parity scheme;
- serialized sidecar/bootstrap bytes;
- filemark map projection;
- catalog rows involved;
- mock tape image, if size permits;
- drive sense data for live failures;
- exact git commit and design-doc versions.

These artifacts make failures reproducible and prevent the most dangerous
outcome: a flaky live-tape test that passes once and is never understood.
