# Reference: Partial File Restore (PFR) on LTO

This page explains the drive mechanics and the arithmetic behind partial file restore: how Remanence reads a byte range out of one file on tape without reading the rest of the object. The format rules themselves are in the REM-OBJECT specification (`specs/publication/`); If this page and the specification differ, follow the specification.

Sources are listed at the bottom. Anything on this page that is not cited should be checked against the IBM LTO SCSI Reference and T10's SCSI Stream Commands standard (SSC-5) before code depends on it.

---

## 1. What "PFR" means

Two operations are often grouped under the same acronym. They are kept separate here:

| Operation | What the caller asks for |
|--|--|
| **Single-file restore** | File `foo.dat` from object `obj-0042`. |
| **Partial file restore (PFR)** | Bytes `[800 MB, 802 MB)` of file `foo.dat` from object `obj-0042`. |

This page is about the second. Single-file restore is the special case where the byte range is `[0, file_size)`.

---

## 2. What the hardware provides

PFR rests on three things LTO exposes via SCSI:

1. **Logical Block Addresses (LBAs).** Every application-level block
   written to tape gets an LBA, monotonically increasing within a
   partition. The drive maintains the LBA-to-physical-position mapping;
   the application never deals with track numbers or wraps.
2. **LOCATE BLOCK.** The drive can seek directly to a given LBA without
   reading intermediate blocks.
3. **READ POSITION.** The drive reports its current LBA on demand —
   useful for sanity checks and progress.

A fourth optional accelerator exists on LTO-9 and later full-height drives only:

4. **RAO (Recommended Access Order).** The application submits a batch
   of segments — each a partition, a start and an *end* block — and the
   drive returns them reordered for the serpentine track layout. Used
   for bulk multi-target restore, not by single-range PFR. Because it is not
   available on every drive Remanence supports, rem does not use it and
   computes the order itself instead; see §8.

### 2.1 The LOCATE BLOCK command

LOCATE comes in two flavours per SSC-3 (SCSI Stream Commands, the T10 standard for tape drives):

| Form | Opcode | Block-address width | Notes |
|--|--|--|--|
| `LOCATE(10)` | `2Bh` | 32-bit | Legacy. 32 bits is enough to address LTO-9 at the block sizes rem supports (256 KiB to 1 MiB) but not small-block formats. |
| `LOCATE(16)` | `92h` | 64-bit | The form rem uses. |

The byte layout of the `LOCATE(16)` command (its CDB, the command descriptor block, which is the SCSI command packet) is defined in SSC-5 and in the IBM LTO SCSI
Reference (the LOCATE(16) table); take it from there rather than from this page. What matters
here is which options rem uses: it sends a logical block address in the command's 8-byte
address field, with every selector bit (destination type, change partition, immediate and
block address mode) set to zero.

That means logical block addressing in partition 0, waiting for completion. Physical positions
are not portable across drives, generations, or even read passes within
the same cartridge. Logical block addresses are the durable identifier.

### 2.2 Block sizes and what an LBA actually addresses

- **An LBA addresses one *application-level block*,** also called a logical
  record: the unit the host writes, whatever its size. If the host writes
  1 MiB records, each LBA = 1 MiB of user data. The size of a block is
  separate from the size of a WRITE command, because in fixed-block mode one
  WRITE command can transfer several consecutive blocks (rem batches writes
  this way).
- **The drive's internal codeword unit is different and not application-
  visible.** The drive internally groups host blocks into "data sets"
  for ECC and compression. An app doing READ on LBA N gets the bytes the
  app wrote at LBA N regardless of what the drive did internally.
- **Block size is variable per write call** by default. The app can also
  set a fixed block size via MODE SELECT.
- Practical block sizes for archive apps: **256 KiB to 1 MiB**. Smaller
  blocks cost per-block overhead (sync, ECC framing); larger blocks cost
  over-read during PFR. The drive's own maximum block length is larger (see
  §11), but Remanence writes tapes only at 256 KiB, 512 KiB and 1 MiB.

**Consequence for REM-OBJECT:** in a plaintext copy `chunk_size` equals the
tape block size, so one chunk is one LBA, and positioning to an LBA delivers
exactly one chunk in one block read. An encrypted copy stores each chunk
with a 16-byte tag, so its chunks do not line up with tape blocks (§4.6).

### 2.3 LOCATE latency, real numbers

LTO seek times are much slower than disk seeks. The numbers an
orchestrator should plan against:

| Scenario | LTO-8 | LTO-9 | Notes |
|--|--|--|--|
| BOT (beginning of tape) → middle (average locate) | ~60 s | similar | IBM-documented. |
| End-to-end (BOT → EOT, end of tape) | ~100–102 s | similar | Tape is ~1 km of physical media. |
| Typical inter-file seek | 10–100 s | 10–100 s | Depends on distance + drive thermal calibration state. |
| With ordered access (batch) | n/a (software ordering only) | ~50% reduction | Per CERN testing, for recalls under 500 targets. |
| Reposition after a stop | 2.50 s mean | similar | HP LTO-6 technical reference. ~750 MB of foregone streaming at 300 MB/s. |

**Practical rule of thumb:** when the drive has to LOCATE, a cold byte-range PFR on an
already-loaded tape costs **one LOCATE (about 60 s on average from BOT to the middle of the tape,
10–100 s between files, and about 100 s end to end, per the table above)
plus the chunk read (sub-second for ≤1 MiB)**. Loading a fresh tape
adds the library MOVE + drive LOAD time on top (LTO-9 first-load
calibration alone can take up to two hours; subsequent loads are
~30 s).

Two corollaries:

- **PFR is fast only when the tape is already loaded.** For an
  orchestrator restoring from cold storage, queue depth matters more
  than individual-range latency.
- **Batching matters.** Restoring 100 byte-ranges from one tape in a
  planned order is much faster than 100 separate PFR calls —
  partly from shorter travel, and partly from avoiding the 2.5 s
  reposition after each stop.

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

rem writes file marks at coarse boundaries (one per object, plus
the structural files described in the tape layout reference) for
recoverability. A range read positions to its first block by the means §4.1 describes:
forward block SPACE when the drive is already close, LOCATE by LBA otherwise, and REWIND
plus filemark SPACE only when the absolute start is unknown.

---

## 3. The PFR problem stated precisely

Given:
- tape `T` is loaded in a drive;
- object `O` is stored on `T` as a run of fixed-size blocks;
- file `F` lies within object `O` and has size `Z`;
- the caller asks for `n` bytes starting at byte offset `s` of `F`, where `0 ≤ s` and `s + n ≤ Z` (the sum computed with checked arithmetic);

the result is a byte stream of exactly `n` bytes, in a time that depends on tape position and on the number of blocks covered, not on the size of the file.

What a range read verifies depends on the copy. A plaintext copy on a parity-protected tape is covered by the parity layer's per-block CRCs (checksums), which detect damage but do not authenticate. An encrypted copy authenticates each chunk it releases (§4.6). A plaintext copy without the parity layer has no per-range integrity. A range read that does not cover the whole file cannot check the file's whole-file hash. REM-OBJECT section 6.5 states these cases, and a restorer reports which one applies.

These are the checks the formats make possible. The daemon's ranged read RPC streams the
covering blocks as opaque bytes, trimmed to the range, and does not check sidecar CRCs on
that path. An encrypted range is decrypted and authenticated by the client, as described in
the [extract-stream protocol reference](reference-extract-stream-protocol.md).

---

## 4. The mapping arithmetic

REM-OBJECT stores an object as a sequence of body blocks of `chunk_size` bytes each. In a plaintext copy on tape, one body block is one tape block, and sections 4.1 to 4.5 describe that case. Section 4.6 gives the mapping for encrypted copies. Each regular file's payload starts on a block boundary, and the object's manifest records, for every file, `first_chunk_lba` (the index of the file's first body block within the object) and `size_bytes`. The catalog holds the same two values per file, so a range read does not need to fetch the manifest first. Nothing in the stream is compressed, so a byte offset in a file maps to a block by division alone.

Hardlinked names store no content of their own. A PFR request on a hardlinked name uses the `first_chunk_lba` and `size_bytes` of the file it links to.

### 4.1 From byte range to block range

With `C = chunk_size`, `L = first_chunk_lba`, `Z = size_bytes`, `chunk_count = ceil(Z / C)` (the number of body blocks the file's payload covers), and a request for `n` bytes at offset `s`:

1. **Bound check.** `s + n ≤ Z`, computed with checked arithmetic. Reject the request otherwise. If `n = 0` (and `s ≤ Z`) the result is the empty range and nothing is read. The formulas below apply to `n > 0`.
2. **First and last body block:**

   ```
   b_first = L + floor(s / C)
   b_last  = L + floor((s + n - 1) / C)
   blocks_to_read = b_last - b_first + 1
   ```

3. **Find the tape position** of body block `b_first`. The catalog gives where the object's first block is; body block `b` is `b` blocks after it.
4. **Position the drive** at that block. When the file's start position on the tape is known, the daemon's reader spaces forward by blocks if the drive is already inside the same tape file at or before the target, uses an absolute LOCATE(16) otherwise, and falls back to REWIND followed by filemark and block SPACE when no absolute start is known. In the LOCATE case this is one LOCATE; the remaining blocks are at consecutive positions, so no further positioning is needed. The worked examples below count one LOCATE for that reason.
5. **Read `blocks_to_read` blocks.**
6. **Trim.** Drop `head_drop = s - floor(s / C) * C` bytes from the start of the first block, then emit exactly `n` bytes. Bytes after the requested range, including whatever follows the file's last byte in its final block, are discarded. The final block of a file holds `Z - (chunk_count - 1) * C` bytes of the file, followed by unrelated stream bytes, so trimming always counts from `n` and `Z`, never from block boundaries.

### 4.2 A two-block example

A caller asks for `[800 MiB, 802 MiB)` of `foo.dat`, with `chunk_size` = 1 MiB:

```
b_first        = L + 800 MiB / 1 MiB = L + 800
b_last         = L + (802 MiB - 1) / 1 MiB = L + 801
blocks_to_read = 2

LOCATE(16) to body block L + 800
READ block (bytes [800 MiB, 801 MiB) of the file)
READ block (bytes [801 MiB, 802 MiB) of the file)

head_drop = 800 MiB - 800 * 1 MiB = 0
Emit 2 MiB.
```

Two blocks, one LOCATE.

### 4.3 A sub-block example

A caller asks for `[800.25 MiB, 800.75 MiB)`, with `chunk_size` = 1 MiB:

```
b_first        = L + 800
b_last         = L + (800.75 MiB - 1 B) / 1 MiB = L + 800
blocks_to_read = 1

LOCATE(16) to body block L + 800
READ block (bytes [800 MiB, 801 MiB) of the file)

head_drop = 800.25 MiB - 800 MiB = 0.25 MiB
Drop the first 0.25 MiB and emit 0.5 MiB; the last 0.25 MiB of the block is discarded.
```

One LOCATE, one block read, and half of the block read is discarded. A range that straddles a block boundary reads more: `[0.75 MiB, 1.25 MiB)` with the same `chunk_size` covers two blocks, reads 2 MiB and discards 1.5 MiB. This is why `chunk_size` is a choice worth making deliberately (section 5).

### 4.4 A short final block

`foo.dat` has `Z` = 1.5 MiB and `chunk_size` = 1 MiB, so it occupies two blocks and the second holds 0.5 MiB of the file. A caller asks for `[1.25 MiB, 1.5 MiB)`, which lies entirely in that second block:

```
b_first        = L + 1.25 MiB / 1 MiB = L + 1
b_last         = L + (1.5 MiB - 1 B) / 1 MiB = L + 1
blocks_to_read = 1

LOCATE(16) to body block L + 1
READ block (bytes [1 MiB, 1.5 MiB) of the file, then unrelated stream bytes)

head_drop = 1.25 MiB - 1 MiB = 0.25 MiB
Drop the first 0.25 MiB and emit n = 0.25 MiB; everything after byte 1.5 MiB of the file is discarded.
```

Counting from `n` rather than from the end of the block keeps the result correct when the final block holds other bytes after the file's last byte.

### 4.5 Seek granularity

The smallest amount that can be read from tape for one range is one block, so `chunk_size` is the seek granularity. A read covers `blocks_to_read` blocks and discards `blocks_to_read * C - n` bytes, which is less than `2 * C` for any range. Many small ranges that fall in the same block are cheaper as one read.

### 4.6 Encrypted copies

The mapping in 4.1 gives the inner chunk `b` of the plaintext stream. In an encrypted copy (REM-ENCRYPT), inner chunk `b` is stored as encrypted chunk `b`, which is `C + 16` bytes (the chunk plus a 16-byte authentication tag), after a header of `F` bytes (`F = 128 + key_frame_len + metadata_frame_len`):

```
cipher_offset(b) = F + b * (C + 16)
cipher_len       = C + 16
```

Because of the 16-byte tags, stored chunks drift against tape-block boundaries, and a run of `k` chunks occupies at most `k + ceil(16 * k / C) + 1` stored blocks (REM-ENCRYPT section 6.4). Encrypted PFR needs the object's key and one read of the metadata frame to learn `F`. The restorer authenticates each chunk and releases only chunks whose tag verifies, then trims as in 4.1. A failure after some chunks were released aborts the range, and the restorer reports it.

---

## 5. Chunk size: how to pick it

| Chunk size | Chunks per GiB | Over-read for a range inside one block | Remarks |
|--|--|--|--|
| 256 KiB | 4,096 | under 256 KiB | The REM-OBJECT default. |
| 512 KiB | 2,048 | under 512 KiB | |
| 1 MiB | 1,024 | under 1 MiB | Fewer blocks per GiB; larger over-read. |

Remanence writes tapes only at these three block sizes (REM-PARITY section 8.4). The drive can accept larger blocks (see §11), and REM-OBJECT itself allows any positive multiple of 512 bytes, but a Remanence tape uses one of the three. A range that straddles a block boundary reads two blocks and discards less than `2 * chunk_size` in total.

The default is 256 KiB. Smaller blocks cost per-block overhead on the drive (sync and ECC framing); larger blocks cost over-read on every small range. For most PFR work, such as video frame ranges or slices of a scientific dataset, an over-read of a few hundred KiB is small next to the 10 to 100 seconds a LOCATE costs. The object's `chunk_size` is recorded with the object, so readers do not assume it. On tape it equals the tape block size of the object's tape file.

---

## 6. Compression interaction

Compression can happen in the drive. REM-OBJECT itself does not compress.

### 6.1 LTO hardware compression (SLDC / NLZ-1)

- Built into the drive. Toggleable via MODE SELECT page 0Fh (the Data Compression page) or via the COMPRESSION bit in MODE SELECT.
- Operates on the application's block stream and produces fewer physical bytes on tape, but **LBAs remain the same**. If the application writes LBA 42 and reads LBA 42 back, it gets the same bytes.
- For already-compressed data (video, archives, encrypted data), hardware compression is a no-op and in the worst case expands the data slightly.

### 6.2 Format-level compression

None. REM-OBJECT performs no compression, because the payload is mostly already-compressed media and compressing the stream would make a file's offset depend on decompressing everything before it. That would remove the closed-form mapping of section 4.

### 6.3 Posture

Hardware compression is off by default. Compressing already-compressed data costs drive CPU and gains no capacity, and disabling it removes a vendor-specific variable from what is stored on tape.

---

## 7. Encryption interaction

LTO supports hardware AES-256-GCM via SECURITY PROTOCOL IN/OUT. Like hardware compression, hardware encryption is **LBA-transparent**: LBA N reads back the same plaintext bytes that were written, and the drive does the GCM encrypt and decrypt internally.

With hardware encryption, reading one LBA returns that block's plaintext, so block N can be read without reading block N-1.

Encrypted REM-OBJECT copies made with REM-ENCRYPT are a separate mechanism, applied by the software before the bytes reach the drive. Their stored chunks are mapped from the plaintext byte range as section 4.6 and REM-ENCRYPT section 6 define.

---

<!-- code-anchor: crates/remanence-order crates/remanence-scsi/src/read_end_of_wrap_position.rs crates/remanence-scsi/src/report_supported_opcodes.rs crates/remanence-api/src/read_plan.rs crates/remanence-api/src/calibration.rs @ 1dd451b2 -->
## 8. Read ordering and batch restore

Ordering is not a PFR feature in itself — it is a multi-target restore
accelerator. It matters here because PFR is what turns a restore from a
few large objects into many small scattered ranges, which is the case
where ordering begins to pay.

### 8.1 What the drive feature does

Tape drives from LTO-9 onward offer Recommended Access Order (RAO). The
application sends the drive a list of the segments it wants, and the
drive returns the same list sorted to minimise total positioning time.

Each segment is a *User Data Segment*, described by a partition number,
a **beginning** logical object identifier and an **ending** logical
object identifier. Both ends are required. The drive rejects a descriptor
whose end precedes its beginning, and the reason both are needed is
visible in what the drive returns: its estimated locate time is measured
from the *end* of the previous segment to the *beginning* of the next.
The cost of reading B after A depends on where A finishes, because that
is where the head is left — and on serpentine media a long segment may
finish on a different wrap, travelling in the opposite direction.

Published gains are substantial: CERN measured positioning reductions
around 50% for recalls of fewer than 500 files.

### 8.2 Why rem does not use it

**Built, and live behind an advisory RPC.** `READ END OF WRAP POSITION` is
implemented in `remanence-scsi`, and the wrap-order computation and cost
model ship as `remanence-order`, a dependency-free crate. `remanence-api`
harvests a drive's wrap map once per load and answers it through the gRPC
`ReadPlanService.PlanBatchRead` RPC (see [architecture
overview](architecture-overview.md#read-order-planning)) — a plan that
never touches media and commits nothing, so the read contract in §8.3
below is not affected. No in-tree CLI issues this RPC yet; it exists today for
an external orchestrator client. Remanence does not yet have a batched-execution RPC (§8.4);
planning an order and executing a batch of reads are different things,
and only the former exists.

RAO is available on **LTO-9 and later full-height drives only**. It is
not available on LTO-9 half-height, nor on LTO-8 or earlier. The
reference LTFS driver encodes the same restriction.

rem therefore computes the order itself, from the cartridge's real wrap
boundaries and the block ranges the caller supplies.

Those boundaries come from `READ END OF WRAP POSITION` (`A3h/1Fh/45h`,
long form), which reports the last logical object on every wrap holding
user data. It is a different command from the Recommended Access Order
feature above, and is supported from LTO-7 onward including half-height
drives. rem reads it once per load and caches the result.

The timing matters, and it is dictated by the standard rather than
chosen. The command's return data "is valid at load" and "becomes stale
on any write operation", and nothing in the standard says re-issuing it
refreshes anything. The drive takes the snapshot when the cartridge
loads; a write invalidates it for the remainder of that load. So rem
harvests **after load and before any write**, and a volume written during
a load stays uncalibrated until its next load. A restore from a cartridge
in the same load it was written in therefore gets no ordering benefit.

Losing the cache costs one command at the next load, since the map is
re-derivable from the medium.

rem reads the boundaries from the drive instead of deriving them from
each generation's published capacity, because published capacity is not
exact. IBM gives two figures for LTO-8, 12,000 and 11,600 GB, and directs
system designs to read the application design capacity at runtime. The
resulting error is not proportional: wrap parity decides whether a block
sits near or far along its wrap, so an error large enough to shift the
wrap index by one places that block at the opposite end of the cartridge.
An ordering built on approximate boundaries can therefore be no better than
ascending block order, or worse than not ordering at all.

Ordering is available on LTO-7 and later. LTO-6 and earlier predate the
command; for those, rem reports the volume unsupported rather than
ordering on an approximation that can be worse than not ordering.

Volumes written with drive compression enabled are also excluded, because
the number of blocks in a wrap then depends on how well each wrap's
contents compressed, which cannot be computed from anything recorded.

### 8.3 The read contract

The read contract is strict. Read requests are serialized through the drive's actor, and the read path does not reorder them. Each `ReadObjectRange` and `ReadFile` call reads one target. Concurrent calls from different clients are served one at a time, and the order between them is not specified.

Ordering is offered as advice: rem is asked for a good order, and the
caller then issues its own reads in whatever order it chooses. A plan
that has gone stale is suboptimal, never incorrect, so no session or
validity window attaches to it.

### 8.4 Batched execution is not implemented

Remanence does not yet have an RPC that accepts a batch of read targets in one session and executes them together. `PlanBatchRead` (§8.2) only plans an order and never reads.

---

## 9. Prior art

### 9.1 LTFS extent lists

LTFS (which rem does not implement)
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

**Why REM-OBJECT uses a per-file block index instead:**

1. LTFS's `byteoffset` mechanism exists because LTFS allows file ranges to share tape blocks with other files' ranges (the cost of doing small writes without rewriting whole blocks). REM-OBJECT starts every file payload on a block boundary, which removes the intra-block-offset case at the cost of some wasted space in the last block of each file.
2. LTFS files can be discontiguous (multi-extent). A REM-OBJECT file is one contiguous run of blocks, so the index is a first block and a size, not a list of extents.
3. Giving up flexibility for simpler PFR arithmetic suits an immutable archive.

### 9.2 tar with sidecar index (ratarmount, tar-split)

Ratarmount builds a SQLite index `.<archive>.index.sqlite` mapping file paths to their byte offsets inside the tar stream. With the index, tar members can be opened and read at random offsets without scanning the archive. That works on a file or disk. A tape needs more: the reader must know which tape block holds a given byte and what to discard after reading it.

REM-OBJECT is a plain pax tar stream, so standard `tar` reads it. It also carries its own index and alignment: the manifest records each file's first block and size, and every file payload starts on a block boundary, so a byte range maps to whole tape blocks by the arithmetic in section 4 without an external index file.

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

## 10. Summary: how REM-OBJECT supports PFR

1. **One chunk per tape block in a plaintext copy.** Every body block is `chunk_size` bytes and is one tape block, so a range read positions once and then reads consecutive blocks. An encrypted copy stores `C + 16`-byte chunks and uses the mapping in section 4.6.
2. **`chunk_size` recorded with the object.** The default is 256 KiB. Readers take the value from the catalog, the bootstrap or the object's own header rather than assuming it.
3. **Payloads start on block boundaries.** A file's first body block is `payload offset / chunk_size`, so the position of any byte in the file follows from arithmetic.
4. **Per-file index.** The manifest and the catalog record `first_chunk_lba` and `size_bytes` for every file, and the same values serve plaintext and encrypted copies of an object.
5. **No format-level compression.** Hardware compression is off by default.
6. **Hardware encryption is transparent** to block addressing, and REM-ENCRYPT maps plaintext ranges to stored ranges for software-encrypted copies (section 4.6).

---

## 11. Maximum block size

The hardware reports a maximum block size through the **READ BLOCK LIMITS** response (IBM LTO SCSI Reference §5.2.17.1 / Table 78), not through MODE SENSE. There are two distinct values: the *reported* `MAXIMUM BLOCK LENGTH LIMIT` field, which Table 78 documents as `0x80_0000` (8 MiB) on LTO-9, and the *supported* maximum from §4.11, where unencrypted cartridges accept up to `0xFF_FFFF` (about 16 MiB - 1) per Note 15 even though the field does not report it. Layer 3a's `read_config()` issues READ BLOCK LIMITS at session open and stores the reported value on `DriveHandle`. Callers query that value rather than hard-coding a portable cap, and should expect that the drive may accept a larger block in practice.

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
- mxmlnkn. *ratarmount.* <https://github.com/mxmlnkn/ratarmount>
- Drastic Technologies. *Net-X-Copy Partial File Restore.* <https://www.drastic.tv/support-59/supporttipstechnical/44-net-x-copy-partial-file-restore>
- Imagen. *PFR (Partial File Restore) and Segments.* <https://knowledge.imagen.io/pfr-partial-file-restore-and-segments>
- Spectra Logic. *PFR System Overview.* <https://rio.spectrawiki.com/en/install/pfr>
- Quantum. *Timecode-Based Partial File Retrieval 2.0.* <https://qsupport.quantum.com/kb/flare/Content/stornext/SN6_PDFs/Partial_File_Retrieval_Users_Guide.pdf>
- Petertc. *Estimating Magnetic Tape Locate Time.* <https://petertc.medium.com/estimating-magnetic-tape-locate-time-32a74738cdf2>
- Wikipedia. *Linear Tape-Open.* <https://en.wikipedia.org/wiki/Linear_Tape-Open>

Numbers on this page to verify against the IBM LTO SCSI Reference and the
MSL3040 before code depends on them: the LOCATE(16) byte layout in §2.1
and the LTO-9 maximum block size in §2.2. Everything else is from public documentation or arithmetic.
