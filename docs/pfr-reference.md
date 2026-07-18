# Reference: Partial File Restore (PFR) on LTO

**Status:** Working reference, not a specification. Written during the
format's design phase, and some of it predates the shipped names — the
design-phase "rem-chunked-v1" became the `rao-v1` object format defined by
the published RAO Format Specification (`specs/publication/`). Where this
document and the published specification disagree, the specification wins;
this document exists to explain the drive mechanics and arithmetic behind
the format's range-addressing design.

Sources are listed at the bottom. Anything that looks definitive but is
not cited should be verified against the IBM LTO SCSI Reference and the
T10 SSC-5 standard before code depends on it.

---

## 1. What "PFR" actually means

Two distinct operations get bundled under the same acronym. Keep them
separate:

| Operation | What the caller asks for | Capability tier (per spec v0.3 §5.3) |
|--|--|--|
| **Single-file restore** | Give me file `foo.dat` from object `obj-0042`. | `FileAddressable` (Tier 1) |
| **Partial file restore (PFR)** | Give me bytes `[800 MB, 802 MB)` of file `foo.dat` from object `obj-0042`. | `ByteRangeAddressable` (Tier 2) |

This document is about Tier 2. Tier 1 falls out as a degenerate case
(byte range = `[0, file_size)`).

---

## 2. The hardware substrate

PFR is enabled by three things LTO actually exposes via SCSI:

1. **Logical Block Addresses (LBAs).** Every application-level block
   written to tape gets an LBA, monotonically increasing within a
   partition. The drive maintains the LBA-to-physical-position mapping;
   the application never deals with track numbers or wraps.
2. **LOCATE BLOCK.** The drive can seek directly to a given LBA without
   reading intermediate blocks.
3. **READ POSITION.** The drive reports its current LBA on demand —
   useful for sanity checks and progress.

A fourth optional accelerator exists on LTO-9 only:

4. **oRAO (open Recommended Access Order).** The application submits a
   batch of LBAs the drive should visit; the drive returns a reordered
   list optimised for the serpentine track layout. Used by orchestrators
   doing bulk multi-file restore, not by single-range PFR. (LTO-9
   full-height only; not in half-height drives.)

### 2.1 The LOCATE BLOCK command

LOCATE comes in two flavours per SSC-3:

| Form | Opcode | Block-address width | Notes |
|--|--|--|--|
| `LOCATE(10)` | `2Bh` | 32-bit | Legacy; insufficient for modern tape capacities. |
| `LOCATE(16)` | `92h` | 64-bit | The form rem will use. |

`LOCATE(16)` CDB shape (from SSC-3 §6.6; verify against the latest
SSC-5 and the IBM LTO SCSI Reference before coding):

| Byte | Field | Notes |
|--|--|--|
| 0 | opcode = `92h` | LOCATE(16) |
| 1 | `[reserved:5 | BT:1 | CP:1 | IMMED:1]` | BT=0 → logical block; BT=1 → physical position. CP=1 → change partition (uses byte 3). IMMED=1 → return immediately, completion async. |
| 2 | reserved | |
| 3 | partition number | Used only if CP=1. |
| 4..11 | 8-byte block address | Big-endian. The LBA to seek to. |
| 12..14 | reserved | |
| 15 | CONTROL | Standard SCSI control byte. |

**rem will always set BT=0 (logical addressing).** Physical positions
are not portable across drives, generations, or even read passes within
the same cartridge. Logical block addresses are the durable identifier.

### 2.2 Block sizes and what an LBA actually addresses

This is the part the spec v0.3 §5.5 example got wrong. The correction:

- **An LBA addresses one *application-level block*** — the unit the host
  wrote in a single WRITE command. If the host writes 1 MiB chunks, each
  LBA = 1 MiB of user data. If the host writes 64 KiB records, each LBA
  = 64 KiB.
- **The drive's internal codeword unit is different and not application-
  visible.** The drive internally groups host blocks into "data sets"
  for ECC and compression. An app doing READ on LBA N gets the bytes the
  app wrote at LBA N regardless of what the drive did internally.
- **Block size is variable per write call** by default. The app can also
  set a fixed block size via MODE SELECT, but variable is the norm for
  archive workloads.
- Practical block sizes for archive apps: **256 KiB to 1 MiB**. Smaller
  blocks cost per-block overhead (sync, ECC framing); larger blocks cost
  intra-block scan time during PFR. The LTO-9 maximum logical block size
  is in the multi-MiB range but the precise cap is drive-dependent.

**Implication for rem-chunked-v1:** if we set chunk_size == block_size,
then one chunk == one LBA. LOCATE(16) to an LBA delivers exactly one
chunk in one block read. This is the design rem-chunked-v1 adopts;
the spec example in v0.3 §5.5 is aligned with this.

### 2.3 LOCATE latency, real numbers

LTO seek times are unforgiving compared to disk. The numbers an
orchestrator should plan against:

| Scenario | LTO-8 | LTO-9 | Notes |
|--|--|--|--|
| BOT → middle (average locate) | ~60 s | similar | IBM-documented. |
| End-to-end (BOT → EOT) | ~100–102 s | similar | Tape is ~1 km of physical media. |
| Typical inter-file seek | 10–100 s | 10–100 s | Depends on distance + drive thermal calibration state. |
| With oRAO (LTO-9 batch) | n/a | 30–70% reduction | Per CERN testing. Up to 73% on first-byte time per IBM. |

**Practical rule of thumb:** a cold byte-range PFR on an already-loaded
tape costs **one LOCATE (~30–60 s typical, up to ~100 s worst case)
plus the chunk read (sub-second for ≤1 MiB)**. Loading a fresh tape
adds the library MOVE + drive LOAD time on top (LTO-9 first-load
calibration alone can take up to two hours; subsequent loads are
~30 s).

Two corollaries:

- **PFR is fast-ish only if the tape is already loaded.** For an
  orchestrator restoring from cold storage, queue depth matters more
  than individual-range latency.
- **Batching wins.** Restoring 100 byte-ranges from one tape in oRAO
  order is dramatically faster than 100 separate PFR calls.

### 2.4 File marks

LTO supports file marks as in-line separators. Each file mark consumes
one block of tape space and bumps a counter readable via READ POSITION.
They are useful as coarse navigation aids and as recovery checkpoints,
but they are **not the right primitive for PFR**:

- File-mark traversal is faster than block reading but still measured in
  seconds for long distances.
- File marks have no associated byte-range or content metadata. They're
  unlabelled.
- LOCATE(16) by LBA bypasses file marks entirely.

rem will write file marks at coarse boundaries (one per object, plus
the catalog-block separators per spec v0.3 §5.1) for recoverability,
but **PFR seeks happen via LOCATE-by-LBA**, not by SPACE-by-file-mark.

---

## 3. The PFR problem stated precisely

Given:
- tape `T` is loaded in some drive (we have a tape position state machine
  that says so);
- object `O` lives on `T` at LBA range `[O.start_lba, O.end_lba)`;
- file `F` lives within object `O` at file-relative byte range
  `[F.start, F.end)`;
- caller asks for bytes `[U_start, U_end)` of `F` (where `0 ≤ U_start <
  U_end ≤ F.size`);

return a byte stream of exactly `U_end − U_start` bytes corresponding
to the requested range, with end-to-end integrity (no corruption) and
in a time that depends on tape position and chunk count, not file size.

---

## 4. How rem solves it (the mapping math)

The format must expose two indexes (both stored in the on-tape catalog
per spec v0.3 §5.2):

**Per-file index** (one entry per file inside an object):

```
FileIndexEntry {
  file_id:        FileId,    // opaque, format-defined
  file_offset:    u64,       // byte offset within the object
  file_size:      u64,
  first_chunk:    u32,       // index into the per-chunk table below
  chunk_count:    u32,
}
```

**Per-chunk index** (one entry per chunk for every file that supports
Tier 2):

```
ChunkIndexEntry {
  lba:                u64,   // LBA of this chunk on tape
  uncompressed_size:  u32,   // bytes of user data this chunk decodes to
  compressed_size:    u32,   // for stats only; not needed for read
}
```

`uncompressed_size` is fixed (= `chunk_size`) for every chunk except
possibly the last, which may be short. The format pins `chunk_size`
in the object header; the chunk table only needs to record the LBA per
chunk if the size is constant.

### 4.1 The byte-range → chunk-range calculation

Given the indexes and a caller request `[U_start, U_end)`:

1. **Bound check.** `0 ≤ U_start < U_end ≤ F.file_size`. Reject early
   otherwise.
2. **First and last chunk indexes within the file:**

   ```
   first_chunk_within_file = U_start / chunk_size           // integer division
   last_chunk_within_file  = (U_end - 1) / chunk_size
   chunk_count_to_read     = last_chunk_within_file - first_chunk_within_file + 1
   ```

3. **Translate to absolute chunk indexes within the object:**

   ```
   first_abs_chunk = F.first_chunk + first_chunk_within_file
   ```

4. **Look up the LBA of the first chunk:**

   ```
   start_lba = chunk_table[first_abs_chunk].lba
   ```

5. **Issue ONE LOCATE(16) to `start_lba`.** The remaining chunks are at
   consecutive LBAs (assuming the format writes chunks contiguously,
   which rem-chunked-v1 must guarantee), so no further LOCATEs are
   needed.

6. **Read `chunk_count_to_read` blocks.** Each block is one chunk.

7. **Decompress and concatenate.** For zstd-seekable framing, each
   chunk is one independent zstd frame; decompress in sequence.

8. **Compute the actual decompressed byte count.** The last chunk of
   a file may be short — its effective size is `file_size mod
   chunk_size` if that is non-zero, otherwise `chunk_size`. So:

   ```
   F.last_chunk_within_file = (F.file_size - 1) / chunk_size
   short_last_size = F.file_size - F.last_chunk_within_file * chunk_size  // ∈ (0, chunk_size]

   // Iterate every chunk in the inclusive range [first, last], not
   // just the endpoints. `..=` is Rust's inclusive-range syntax;
   // implementations in other languages should produce the same set
   // of integers c such that first_chunk_within_file ≤ c ≤ last_chunk_within_file.
   total_decompressed = 0
   for c in first_chunk_within_file..=last_chunk_within_file:
       if c == F.last_chunk_within_file:
           total_decompressed += short_last_size
       else:
           total_decompressed += chunk_size
   ```

   Equivalently: if the read range does *not* include the file's
   last chunk, `total_decompressed = chunk_count_to_read * chunk_size`.
   If it *does*, swap in `short_last_size` for the last chunk.

   The decoder will also tell you the actual bytes per chunk (zstd
   reports `decompressed_size` per frame); using that as a runtime
   cross-check catches index/data divergence early.

9. **Trim the head and tail:**

   ```
   head_drop = U_start - first_chunk_within_file * chunk_size      // ∈ [0, chunk_size)
   tail_drop = total_decompressed - (head_drop + (U_end - U_start))
   ```

   Drop `head_drop` bytes from the start, drop `tail_drop` bytes from
   the end.

10. **Stream the result** — exactly `U_end - U_start` bytes.

### 4.2 Worked example (the one the spec got wrong)

Caller asks for `[800 MiB, 802 MiB)` of file `foo.dat`, chunk_size = 1 MiB:

```
first_chunk_within_file = 800 MiB / 1 MiB = 800
last_chunk_within_file  = (802 MiB - 1) / 1 MiB = 801
chunk_count_to_read     = 801 - 800 + 1 = 2

LOCATE(16) → chunk_table[F.first_chunk + 800].lba
READ block (chunk 800: bytes [800 MiB, 801 MiB) of file)
READ block (chunk 801: bytes [801 MiB, 802 MiB) of file)

intra_chunk_head_offset = 800 MiB - 800 * 1 MiB = 0
trim_tail               = 2 * 1 MiB - (0 + 2 MiB) = 0

Stream 2 MiB to caller.
```

**Two chunks, two block reads, one LOCATE.** This is the case the spec
§5.5 example walks through — it was wrong in earlier drafts (said "one
block"); v0.3 is now aligned with the math here.

### 4.3 A sub-chunk example

Caller asks for `[800.25 MiB, 800.75 MiB)`, chunk_size = 1 MiB.
File is large; chunk 800 is *not* the last chunk of the file:

```
first_chunk_within_file = 800.25 MiB / 1 MiB = 800
last_chunk_within_file  = (800.75 MiB - 1 B) / 1 MiB = 800
chunk_count_to_read     = 800 - 800 + 1 = 1

LOCATE(16) → chunk_table[F.first_chunk + 800].lba
READ block (chunk 800: bytes [800 MiB, 801 MiB) of file)

(chunk 800 is not the file's last chunk → use full chunk_size)
total_decompressed = 1 MiB
head_drop          = 800.25 MiB - 800 MiB = 0.25 MiB
tail_drop          = 1 MiB - (0.25 MiB + 0.5 MiB) = 0.25 MiB

Drop first 0.25 MiB, drop last 0.25 MiB, stream the middle 0.5 MiB.
```

One LOCATE, one block read, ~50% waste. This is the worst case for a
half-chunk range and motivates picking chunk_size carefully.

### 4.4 A short-last-chunk example

This is the case the trim formula has to special-case. File `foo.dat`
has `file_size = 1.5 MiB`, `chunk_size = 1 MiB`, so:

```
F.last_chunk_within_file = (1.5 MiB - 1 B) / 1 MiB = 1
short_last_size          = 1.5 MiB - 1 * 1 MiB = 0.5 MiB
```

Caller asks for `[1.25 MiB, 1.5 MiB)` — entirely inside the short
last chunk:

```
first_chunk_within_file = 1.25 MiB / 1 MiB = 1
last_chunk_within_file  = (1.5 MiB - 1 B) / 1 MiB = 1
chunk_count_to_read     = 1 - 1 + 1 = 1

LOCATE(16) → chunk_table[F.first_chunk + 1].lba
READ block (chunk 1: bytes [1 MiB, 1.5 MiB) of file — 0.5 MiB of data)

(chunk 1 IS the file's last chunk → use short_last_size, not chunk_size)
total_decompressed = 0.5 MiB
head_drop          = 1.25 MiB - 1 * 1 MiB = 0.25 MiB
tail_drop          = 0.5 MiB - (0.25 MiB + 0.25 MiB) = 0
```

Drop first 0.25 MiB, drop 0 from the end, stream 0.25 MiB. **Correct.**

If we had naively used `total_decompressed = chunks_to_read * chunk_size
= 1 MiB`, `tail_drop` would have been 0.5 MiB — and dropping 0.5 MiB
from a 0.5 MiB stream after a 0.25 MiB head drop would have streamed
*negative* bytes (i.e. crashed or returned empty). This was a real
bug in earlier drafts of this doc and the spec; codex caught it.

### 4.5 seek_granularity()

The `ByteRangeAddressable` trait exposes `seek_granularity() -> u64`.
For rem-chunked-v1 this returns `chunk_size`. The caller uses it as:

- "If my range is smaller than seek_granularity, I'm going to over-read
  by roughly that much. Plan accordingly."
- "If I'm doing many small ranges within the same chunk, batch them
  into one read."

---

## 5. Chunk size: how to pick it

Tradeoffs:

| Chunk size | Index cost (per GiB) | Worst-case over-read | Block read overhead |
|--|--|--|--|
| 64 KiB | ~16 KiB | 64 KiB | Per-block sync/ECC dominates throughput |
| 256 KiB | ~4 KiB | 256 KiB | Sub-second per chunk; ~40 chunks/s |
| 1 MiB | ~1 KiB | 1 MiB | ~360 chunks/s at LTO-9 line rate |
| 4 MiB | ~256 bytes | 4 MiB | ~90 chunks/s |
| 16 MiB | ~64 bytes | 16 MiB | ~22 chunks/s; near upper bound for many drives |

Index cost = LBA per chunk (8 bytes) plus a few bytes of metadata; rounded.

**Default: 1 MiB.** Reasoning:

- Matches typical archive-app block size — drive throughput is comfortable
  at this size.
- Index cost (~1 KiB per GiB) is trivial; a 1 TiB object has a 1 MiB
  chunk index.
- Worst-case over-read on a single PFR call is 1 MiB. For most PFR
  workloads (video frame ranges, scientific dataset slices) this is
  insignificant.
- A 1 MiB block survives LTO compression well — large enough that the
  per-block overhead is small, small enough that the drive's internal
  data-set boundaries don't dominate.

**Override per write session.** The format header records the chunk
size in use. PFR readers consult it; no global setting needed.

---

## 6. Compression interaction

Two compression layers can exist independently:

### 6.1 LTO hardware compression (SLDC / NLZ-1)

- Built into the drive. Toggleable via MODE SELECT page 0Fh (the Data
  Compression page) or via the COMPRESSION bit in MODE SELECT.
- Operates on the application's block stream; produces fewer physical
  bytes on tape but **LBAs remain the same**. App writes LBA 42; app
  reads LBA 42 back; same bytes.
- For already-compressed data (zstd, gzip, encrypted), hardware
  compression is a no-op (zero or negative compression ratio in the
  worst case).

### 6.2 Format-level compression (zstd seekable)

- Applied by rem-chunked-v1 before the bytes hit the drive.
- Each chunk is one independent zstd frame, optionally with the
  seekable-format seek table appended for in-chunk skipping (irrelevant
  for our case since one chunk == one tape block already).
- The decoder can decompress chunk N without state from chunks 0..N-1.
- This is what makes "decodable from chunk boundary" (the Tier 2
  contract) cheap.

### 6.3 Recommended posture for rem-chunked-v1

- **Format-level compression: ON, zstd-seekable per chunk.** This is
  the mechanism that makes PFR cheap.
- **Hardware compression: OFF.** Compressing already-compressed data
  wastes drive CPU and yields no capacity benefit. Disabling it also
  removes a vendor-specific variable from the encoding.
- Caller can override per write session (e.g. raw uncompressed
  archival data may benefit from HW compression; the orchestrator
  knows its content best).

### 6.4 zstd seekable in 200 words

The zstd seekable format is a concatenation of independently-compressed
zstd frames plus a trailing skippable frame containing the seek table.

- Skippable frame magic: any value `0x184D2A5?` (low nibble unconstrained).
- Seek-table-trailer magic: `0x8F92EAB1`, mandated to be the last bytes
  of the file so decoders find the table by reading from EOF backwards.
- Seek table entry: 4 bytes Compressed_Size + 4 bytes Decompressed_Size,
  optionally + 4 bytes XXH64 checksum. So **8 or 12 bytes per frame**.
  (The frame *envelope* — magic + skippable header — adds a fixed
  overhead outside the entry array; the seek-table trailer adds 9
  bytes after the entries.)
- Standard `zstd -d` decoder handles seekable files transparently because
  the seek table sits in a skippable frame the decoder ignores.

For rem-chunked-v1, the seekable-format wire layout is **not** what's
on tape (we write one chunk per tape block; there's no in-frame
seeking needed). But the *independent-frame* property is what we use:
each chunk is one zstd frame with no shared dictionary or state with
its neighbours, so the decoder can start fresh at any chunk.

---

## 7. Encryption interaction

LTO supports hardware AES-256-GCM via SECURITY PROTOCOL IN/OUT. Like
hardware compression, encryption is **LBA-transparent**: LBA N reads
back the same plaintext bytes you wrote, the drive does the GCM
encrypt/decrypt internally.

But there is a subtlety for PFR:

- LTO drives encrypt with **per-block GCM**, where the IV / nonce is
  derived from the LBA. This means: each tape block has its own
  authentication tag. Reading one block decrypts and authenticates
  exactly that block.
- This is good for PFR — we can read chunk N without reading chunk
  N-1 — but it puts a constraint on the chunk == block invariant.
  If we ever broke that (multiple chunks per block, or chunks
  spanning blocks), we'd need to read the whole containing block
  and discard.

**Conclusion:** the "one chunk per tape block" rule for rem-chunked-v1
isn't just a convenience — it's a *correctness* requirement for
encrypted tapes. The format header should refuse encryption + chunks-
per-block > 1.

---

## 8. oRAO and batch restore

oRAO is not a PFR feature — it's a multi-file restore accelerator.
But worth understanding for the orchestrator's benefit:

- The application sends the drive a list of (file_id, partition,
  start_lba) tuples it wants to read.
- The drive returns the same list reordered by tape-traversal-
  efficient order (taking serpentine track wrap into account).
- The application then reads in that order; the drive's internal
  predictive seek minimises wasted motion.
- Quoted gains: 30–70% positioning reduction (CERN), up to 73%
  first-byte-time reduction (IBM).
- LTO-9 full-height only. Not available on LTO-9 half-height or
  LTO-8/earlier.

For rem this is a future opt-in feature, not a transparent
optimisation. The v0.3 contract (`docs/spec-v0.3.md` §3.2, §7.2, §9.0)
is strict: Layer 3a does not queue or reorder, and individual
`ReadRange` calls execute in the order the caller issues them. If
oRAO ever becomes relevant (LTO-9 full-height bays added, or LTO-10
brings oRAO to half-height), it will land as a **new explicit RPC**
— `ReadService.BatchReadRange` per spec §9.0 — that the orchestrator
opts into for a batch of read targets against one session. Inside
that RPC, rem can submit the batch to the drive via `Receive
Recommended Access Order` and stream results back in drive-
recommended order. Callers of plain `ReadRange` retain the strict
no-reorder contract regardless.

---

## 9. Prior art

### 9.1 LTFS extent lists

LTFS (which rem **does not implement** — see spec v0.3 §5 / Appendix A)
solves a similar mapping problem with a per-file extent list, each
entry being:

| Field | Meaning |
|--|--|
| `partition` | Tape partition ID. |
| `startblock` | First LBA of this extent. |
| `byteoffset` | Byte offset within `startblock` where the extent's valid bytes begin (< block size). |
| `bytecount` | Number of bytes in this extent. |
| `fileoffset` | Offset within the file where this extent begins. |

A file can have multiple extents (interleaved writes, holes, etc.) and
maps any file byte to a unique (LBA, intra-block offset) pair.

**Why rem uses a chunk table instead:**

1. LTFS's `byteoffset` mechanism exists because LTFS allows file ranges
   to share tape blocks with other files' ranges (the cost of doing
   small writes without rewriting whole blocks). rem-chunked-v1 enforces
   chunk == block alignment by construction, eliminating the
   intra-block-offset case at the cost of slight space waste on the
   last block of each file.
2. LTFS files can be discontiguous (multi-extent). For an archive
   format we control the writer for, we can guarantee contiguous chunk
   sequences per file, making the index a flat array of LBAs rather
   than a list of (start, count) extents.
3. Trading flexibility for simpler PFR math is the right deal in an
   immutable-archive context.

### 9.2 tar with sidecar index (ratarmount, tar-split)

Ratarmount builds a SQLite index `.<archive>.index.sqlite` mapping
file paths to their byte offsets inside the tar stream. Once built,
the index enables file-level seeking in under a second.

This is **Tier 1 only** — it gets you to the start of a file inside
a tar, but tar has no notion of intra-file chunk boundaries. To get
Tier 2 (PFR) on top of tar, you'd need to either:

- Wrap tar in zstd-seekable (chunked compressed envelope) and index
  the zstd frames, not the tar entries; or
- Write a sidecar PFR index alongside the tar index, with byte
  offsets per fixed-size chunk inside each file.

`pax-tar-v1` in rem is the first option-shaped solution: pax tar
inside the chunked container. The format implementation declares
Tier 1 only (the chunk table is rem's, not tar's; using it for PFR
loses the round-trip guarantee that standard `tar` tooling can read
the bytes).

### 9.3 Spectra Logic / Drastic / Imagen vendor implementations

The broadcast-archive vendors (Drastic Net-X-Copy, Imagen PFR,
Spectra Rio) all implement PFR with the same general shape: a
proxy/index file built at archive time, mapping timecode or
absolute file offsets to byte ranges in the master file. Restore
of a sub-range reads only the required portion off tape (or off
object storage, where the same pattern applies).

The implementations vary in the kind of index (timecode-aware for
broadcast video, raw byte offsets for generic data) and the
retrieval API (range request via HTTP, NAS staging, etc.) but the
underlying tape mechanic — index per chunk, LOCATE to chunk, read
covering chunks, trim — is invariant.

---

## 10. Implications for rem-chunked-v1

Concretely, what the format spec needs to say:

1. **One chunk per tape block.** Hard invariant. Enables one-LOCATE
   PFR and per-block GCM correctness.
2. **Chunk size in the object header.** Default 1 MiB, configurable
   per write session. Recorded in the chunk table header so readers
   don't have to assume.
3. **Chunks contiguous within a file, files contiguous within an
   object.** Allows LBA arithmetic without per-chunk index lookups
   for sequential reads. The catalog still records each chunk LBA
   for resilience.
4. **Per-file index** in the object header: `(file_id, file_offset,
   file_size, first_chunk, chunk_count)`.
5. **Per-chunk index** in the object header: just `lba` per chunk
   (compressed/uncompressed sizes are stats, not needed for read
   correctness given chunk_size is uniform). For a 1 TiB object at
   1 MiB chunks: 1,048,576 chunks × 8 bytes = 8 MiB index.
6. **zstd-seekable per-chunk framing.** Each chunk = one zstd frame.
   No shared dictionary. Per-chunk XXH64 checksum (the seekable
   format's optional 4-byte checksum field is sufficient).
7. **Hardware compression OFF by default.** Format-level zstd does
   the compression.
8. **Hardware encryption transparent.** Per-block GCM matches our
   one-chunk-per-block invariant; no extra format work needed.
9. **CBOR-serialised metadata blocks** for header and chunk table.
   Stable schema, versioned.

---

## 11. Open questions

1. **Index placement.** Do we put the chunk table inside the object
   (at the start, before the data chunks), or only in rem's catalog
   (file mark 0 of the tape)? Tradeoff: in-object placement means the
   object is self-recoverable without the tape catalog, at the cost
   of reading the index from a different LBA than the data. Catalog-
   only means one trip to the catalog block to get all PFR addresses
   for the tape. Lean: both. Catalog has all per-tape indexes; each
   object also embeds its own at object start for resilience.
2. **`oRAO` integration timeline.** Worth wiring into the batch-
   restore API or defer until real demand? Lean: defer.
3. **Maximum chunk_size.** Hardware reports a max via the **READ
   BLOCK LIMITS** response (IBM LTO SCSI Reference §5.2.17.1 /
   Table 78), **not** MODE SENSE. There are two distinct values:
   the *reported* `MAXIMUM BLOCK LENGTH LIMIT` field — Table 78
   documents this as `0x80_0000` (8 MiB) on LTO-9 — and the
   *supported* max from §4.11, where unencrypted cartridges accept
   up to `0xFF_FFFF` (≈16 MiB - 1) per Note 15 even though the
   field doesn't report it. Layer 3a's `read_config()` issues
   READ BLOCK LIMITS at session open and stashes the reported
   value on `DriveHandle`; callers should query that rather than
   hard-code a portable cap, but understand that the drive may
   accept a larger block in practice.
4. **Variable chunk_size within an object.** Last chunk is naturally
   short; is there ever a reason to allow chunks of differing sizes
   mid-object? Lean: no. Uniform chunk_size keeps the math trivial.
5. **Format-level integrity vs hardware ECC.** LTO has strong ECC; do
   we still need per-chunk XXH64? Lean: yes — protects against
   silent corruption between drive and host, e.g. cable / HBA bit
   flips.

---

## Sources

- IBM. *LTO SCSI Reference (GA32-0928-07).* June 2025. <https://www.ibm.com/support/pages/system/files/inline-files/%5BAPPROVED%5D%20LTO%20SCSI%20Reference%20GA32-0928-07%20(EXTERNAL).pdf>
- IBM. *LTO SCSI Reference (GA32-0928-06).* April 2024. <https://www.ibm.com/support/pages/system/files/inline-files/%5BAPPROVED%5D%20LTO%20SCSI%20Reference%20GA32-0928-06.pdf>
- T10. *SCSI Stream Commands — 5 (SSC-5).* <https://www.t10.org/members/w_ssc5.htm>
- SNIA. *LTFS Format Specification, version 2.5.* <https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-%76%32.5-Technical-Position.pdf>
- LTO Program. *LTO-9 product overview.* <https://www.lto.org/lto-9/>
- IBM. *Performance specifications for LTO tape drives.* <https://www.ibm.com/docs/en/ts4500-tape-library?topic=performance-lto-specifications>
- Versity. *Designing a High Performance Tape Archive.* <https://www.versity.com/designing-a-high-performance-tape-archive/>
- Archiware. *Why LTO Tape Drives Never Reach Their Rated Speed.* <https://blog.archiware.com/blog/why-lto-tape-drives-never-reach-their-rated-speed/>
- Facebook. *zstd seekable format specification.* <https://github.com/facebook/zstd/blob/dev/contrib/seekable_format/zstd_seekable_compression_format.md>
- IETF RFC 8478. *Zstandard Compression and the application/zstd Media Type.* <https://www.ietf.org/rfc/rfc8478>
- mxmlnkn. *ratarmount.* <https://github.com/mxmlnkn/ratarmount>
- Drastic Technologies. *Net-X-Copy Partial File Restore.* <https://www.drastic.tv/support-59/supporttipstechnical/44-net-x-copy-partial-file-restore>
- Imagen. *PFR (Partial File Restore) and Segments.* <https://knowledge.imagen.io/pfr-partial-file-restore-and-segments>
- Spectra Logic. *PFR System Overview.* <https://rio.spectrawiki.com/en/install/pfr>
- Quantum. *Timecode-Based Partial File Retrieval 2.0.* <https://qsupport.quantum.com/kb/flare/Content/stornext/SN6_PDFs/Partial_File_Retrieval_Users_Guide.pdf>
- Petertc. *Estimating Magnetic Tape Locate Time.* <https://petertc.medium.com/estimating-magnetic-tape-locate-time-32a74738cdf2>
- Wikipedia. *Linear Tape-Open.* <https://en.wikipedia.org/wiki/Linear_Tape-Open>

Numbers in this document that should be verified against the IBM LTO
SCSI Reference and the live MSL3040 before code depends on them: the
LOCATE(16) byte layout in §2.1; the LTO-9 maximum block size in §2.2;
the per-block GCM IV-derivation claim in §7. Everything else is from
public documentation or self-evident arithmetic.
