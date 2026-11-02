# Partial-file restore on LTO

This page is for someone who writes an orchestrator that restores parts of files, and for
anyone curious about the tape mechanics behind it. It explains how Remanence reads one byte
range of one file from tape without reading the rest of the object: what the drive provides,
the arithmetic that maps a byte range to tape blocks, what the positioning costs, and how
reads can be ordered. The format rules are in REM-OBJECT §6 and REM-ENCRYPT §6; where this
page and a specification differ, the specification is correct. Facts about drives are cited to
the documents listed under [Sources](#sources).

## 1. What "PFR" means

Two operations often share the acronym. This page keeps them apart:

| Operation | What the caller asks for |
| --- | --- |
| Single-file restore | File `foo.dat` from object `obj-0042`. |
| Partial file restore (PFR) | Bytes `[800 MiB, 802 MiB)` of file `foo.dat` from object `obj-0042`. |

This page is about the second. A single-file restore is the special case in which the byte
range is `[0, file_size)`.

## 2. What the drive provides

PFR rests on three things an LTO drive offers through SCSI.

1. Logical block addresses (LBAs). The drive numbers every block and every filemark written to
   a partition in order, from 0 at the beginning of the tape (see
   [LBA](reference-glossary.md#lba)). The drive maps each LBA to a physical position itself; the
   application never deals with tracks or wraps.
2. LOCATE. The drive moves directly to a given LBA without reading the blocks in between.
3. READ POSITION. The drive reports its current LBA on request.

A fourth feature exists on LTO-9 and later full-height drives only: Recommended Access Order
(RAO), in which the application gives the drive a list of segments and the drive returns them
in an order that reduces positioning time (IBM LTO SCSI Reference, section 4.6). It serves
restores of many targets, not a single range. It is not available on every drive Remanence
supports, so Remanence does not use it and computes an order itself instead (section 8).

### 2.1 The LOCATE command

LOCATE has two forms in the T10 SCSI Stream Commands standard (SSC-5), and LTO drives
implement both (IBM LTO SCSI Reference, section 5.2.7):

| Form | Opcode | Address width | Remarks |
| --- | --- | --- | --- |
| `LOCATE(10)` | `2Bh` | 32 bits | Enough for an LTO-9 cartridge at the block sizes Remanence writes, but not at small block sizes. |
| `LOCATE(16)` | `92h` | 64 bits | The form Remanence uses. |

The byte layout of LOCATE(16) is defined in SSC-5 and in IBM's reference. Remanence builds it in
`remanence-scsi/src/locate.rs`: the logical block address goes in the command's 8-byte address
field, and byte 1 is zero. That selects logical block addressing, no change of partition, and
no immediate return, so the command completes only when the tape has arrived. Remanence works
in partition 0 only. Physical positions are not portable across drives or generations, so
logical block addresses are the identifier that lasts.

### 2.2 Block sizes and what an LBA addresses

An LBA addresses one logical block, the unit the host writes, whatever its size. If the host
writes 1 MiB blocks, each LBA holds 1 MiB of data. The size of a block is separate from the
size of a WRITE command: in fixed-block mode one WRITE can carry several consecutive blocks
(IBM LTO SCSI Reference, section 4.11), and Remanence batches its writes this way. The drive
groups blocks internally for error correction and compression, but the application never sees
that grouping. Reading LBA N returns the bytes that were written at LBA N.

By default a drive accepts a different block size on each write; MODE SELECT can fix the size
instead. Remanence always writes in fixed-block mode, at one of three block sizes: 256 KiB,
512 KiB or 1 MiB (REM-PARITY §8.4). Smaller blocks mean more blocks and more commands for the
same data; larger blocks mean more bytes read and thrown away for a small range. The drive
accepts much larger blocks (section 11), but Remanence does not write them.

In a plaintext copy, REM-OBJECT's `chunk_size` equals the tape block size, so one chunk is one
LBA, and positioning to an LBA delivers exactly one chunk in one block read (see
[chunk size and block size](reference-glossary.md#chunk-size-and-block-size)). An encrypted
copy stores each chunk with a 16-byte authentication tag, so its chunks do not line up with
tape blocks (section 4.6).

### 2.3 What positioning costs

Positioning a tape takes far longer than a disk seek. These are the figures the vendors
publish:

| Drive | Measure | Time | Source |
| --- | --- | --- | --- |
| LTO-9 | Average space record time from load point | 45 s | IBM TS4500 performance specifications |
| LTO-8 | Average space record time from load point | 59 s | IBM TS4500 performance specifications |
| LTO-9 | Nominal load-to-ready time, initialized cartridge | 17 s | IBM TS4500 performance specifications |
| LTO-9 | Load-to-ready time, cartridge not yet initialized | 40 to 120 min | IBM TS4500 performance specifications |
| LTO-6 | Maximum access time, beginning to end of tape | 99 s | HP LTO-6 Technical Reference Manual, volume 4 |
| LTO-6 | Mean reposition time | 2.50 s | HP LTO-6 Technical Reference Manual, volume 4 |

The tape inside an LTO-9 cartridge is 1,035 m long (Fujifilm LTO specifications), which is why
a move from one end to the other takes more than a minute. The mean reposition time is the
cost of each stop and restart while streaming.

A cold byte-range read on a tape that is already loaded therefore costs one positioning, which
takes tens of seconds on average and, by the LTO-6 figure above, about 100 seconds for a move
along the whole tape. The read of the covering blocks is small by comparison. Loading a cartridge adds the
library's move and the drive's load. The first load of a new LTO-9 cartridge adds its one-time
media optimization (see [media optimization](reference-glossary.md#media-optimization-and-conditioning)),
which the LTO Program says averages 20 minutes and can take up to two hours.

Two consequences follow. PFR is fast only when the tape is already loaded, so an orchestrator
restoring from cold storage gains more from how it queues requests than from the speed of any
single range. And many ranges from one tape are faster to read in a planned order than in the
order they arrived, because the head travels less (section 8).

### 2.4 Filemarks

A filemark is a separator the drive writes between two tape files (see
[filemark](reference-glossary.md#filemark)). Each filemark takes one logical position, so the
LBA after a filemark is one higher than the filemark's own (REM-PARITY §3.2). Filemarks are
useful for coarse navigation and for recovery, but they are not the right tool for PFR. They
carry no byte-range or content information, and LOCATE by LBA does not need them.

Remanence ends every tape file with exactly one filemark (REM-PARITY §3.1), so every object and
every structural file is delimited, as the [on-tape layout reference](reference-tape-layout.md)
describes. A range read positions to its first block in the way section 4.1 describes: forward
SPACE by blocks when the drive is already close, LOCATE by LBA otherwise, and REWIND followed by
SPACE over filemarks only when the start of the file is not known.

## 3. The PFR problem stated precisely

Given:

- tape `T` is loaded in a drive;
- object `O` is stored on `T` as a run of fixed-size blocks;
- file `F` lies within object `O` and has size `Z`;
- the caller asks for `n` bytes starting at byte offset `s` of `F`, where `0 ≤ s` and
  `s + n ≤ Z`, the sum computed with checked arithmetic;

the result is a stream of exactly `n` bytes, in a time that depends on the tape's position and
on the number of blocks covered, not on the size of the file.

What a range read verifies depends on the copy. A plaintext copy on a tape with parity is
covered by the per-block CRCs (cyclic redundancy checks) in the parity sidecars, which detect
damage but do not authenticate. An encrypted copy authenticates each chunk it releases (section
4.6). A plaintext copy on a tape without parity has no per-range integrity check. A range that
does not cover the whole file cannot check the file's whole-file hash. REM-OBJECT §6.5 states
these cases, and a restorer reports which one applies.

These are the checks the formats make possible. The daemon's ranged read RPC streams the
covering blocks as opaque bytes, trimmed to the range, and does not check sidecar CRCs on that
path. An encrypted range is decrypted and authenticated by the client, as the
[extract-stream protocol reference](reference-extract-stream-protocol.md) describes.

## 4. The mapping arithmetic

REM-OBJECT stores an object as a sequence of body blocks of `chunk_size` bytes each. In a
plaintext copy on tape, one body block is one tape block; sections 4.1 to 4.5 describe that
case, and section 4.6 gives the mapping for encrypted copies. Each regular file's payload
starts on a block boundary. For every file, the object's manifest records `first_chunk_lba`,
the index of the file's first body block within the object, and `size_bytes`. The catalog holds
the same two values for each file, so a range read does not need to fetch the manifest first.
Nothing in the stream is compressed, so a byte offset in a file maps to a block by division
alone.

A hardlinked name stores no content of its own. A PFR request on a hardlinked name uses the
`first_chunk_lba` and `size_bytes` of the file it links to (REM-OBJECT §6).

### 4.1 From byte range to block range

With `C = chunk_size`, `L = first_chunk_lba`, `Z = size_bytes`, `chunk_count = ceil(Z / C)` (the
number of body blocks the file's payload covers), and a request for `n` bytes at offset `s`
(REM-OBJECT §6.1, §6.2):

1. Check the bounds: `s + n ≤ Z`, computed with checked arithmetic, or reject the request. If
   `n = 0` and `s ≤ Z`, the result is the empty range and nothing is read. The formulas below
   apply to `n > 0`.
2. Find the first and last body blocks:

   ```text
   b_first = L + floor(s / C)
   b_last  = L + floor((s + n - 1) / C)
   blocks_to_read = b_last - b_first + 1
   ```

3. Find the tape position of body block `b_first`. The catalog gives the position of the
   object's first block, and body block `b` is `b` blocks after it.
4. Position the drive at that block. When the file's start position on the tape is known, the
   daemon's reader spaces forward by blocks if the drive is already inside the same tape file at
   or before the target, and uses an absolute LOCATE(16) otherwise. When no absolute start is
   known, it rewinds and spaces over filemarks and then blocks. In the LOCATE case this is one
   LOCATE, and the remaining blocks follow at consecutive positions, so no further positioning
   is needed. The worked examples below count one LOCATE for that reason.
5. Read `blocks_to_read` blocks.
6. Trim. Drop `head_drop = s - floor(s / C) * C` bytes from the start of the first block, then
   emit exactly `n` bytes. Bytes after the requested range are discarded, including whatever
   follows the file's last byte in its final block. The final block of a file holds
   `Z - (chunk_count - 1) * C` bytes of the file followed by unrelated stream bytes, so trimming
   always counts from `n` and `Z`, never from block boundaries.

### 4.2 A two-block example

A caller asks for `[800 MiB, 802 MiB)` of `foo.dat`, with `chunk_size` = 1 MiB:

```text
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

```text
b_first        = L + 800
b_last         = L + (800.75 MiB - 1 B) / 1 MiB = L + 800
blocks_to_read = 1

LOCATE(16) to body block L + 800
READ block (bytes [800 MiB, 801 MiB) of the file)

head_drop = 800.25 MiB - 800 MiB = 0.25 MiB
Drop the first 0.25 MiB and emit 0.5 MiB; the last 0.25 MiB of the block is discarded.
```

One LOCATE and one block read, and half of the block read is discarded. A range that straddles
a block boundary reads more: `[0.75 MiB, 1.25 MiB)` with the same `chunk_size` covers two
blocks, reads 2 MiB and discards 1.5 MiB. This is why `chunk_size` is worth choosing
deliberately (section 5).

### 4.4 A short final block

`foo.dat` has `Z` = 1.5 MiB and `chunk_size` = 1 MiB, so it occupies two blocks, and the second
holds 0.5 MiB of the file. A caller asks for `[1.25 MiB, 1.5 MiB)`, which lies entirely in that
second block:

```text
b_first        = L + 1.25 MiB / 1 MiB = L + 1
b_last         = L + (1.5 MiB - 1 B) / 1 MiB = L + 1
blocks_to_read = 1

LOCATE(16) to body block L + 1
READ block (bytes [1 MiB, 1.5 MiB) of the file, then unrelated stream bytes)

head_drop = 1.25 MiB - 1 MiB = 0.25 MiB
Drop the first 0.25 MiB and emit n = 0.25 MiB; everything after byte 1.5 MiB of the file is discarded.
```

Counting from `n` rather than from the end of the block keeps the result correct when the final
block holds other bytes after the file's last byte.

### 4.5 Seek granularity

The smallest amount that can be read from tape for one range is one block, so `chunk_size` is
the seek granularity. A read covers `blocks_to_read` blocks and discards
`blocks_to_read * C - n` bytes, which is less than `2 * C` for any range. Many small ranges
that fall in the same block are cheaper as one read.

### 4.6 Encrypted copies

The mapping in section 4.1 gives inner chunk `b` of the plaintext stream. In an encrypted copy
(REM-ENCRYPT), inner chunk `b` is stored as encrypted chunk `b`, which is `C + 16` bytes long
(the chunk plus a 16-byte authentication tag), after a header of `F` bytes, where
`F = 128 + key_frame_len + metadata_frame_len` (REM-ENCRYPT §6.3):

```text
cipher_offset(b) = F + b * (C + 16)
cipher_len       = C + 16
```

Because of the 16-byte tags, stored chunks drift against tape-block boundaries, and a run of `k`
chunks occupies at most `k + ceil(16 * k / C) + 1` stored blocks (REM-ENCRYPT §6.4). Encrypted
PFR needs the object's key and one read of the metadata frame to learn `F`. The restorer
authenticates each chunk and releases only chunks whose tag verifies, then trims as in section
4.1. A failure after some chunks were released aborts the range, and the restorer reports it.

## 5. Chunk size: how to pick it

| Chunk size | Chunks per GiB | Over-read for a range inside one block | Remarks |
| --- | --- | --- | --- |
| 256 KiB | 4,096 | under 256 KiB | The REM-OBJECT default. |
| 512 KiB | 2,048 | under 512 KiB | |
| 1 MiB | 1,024 | under 1 MiB | Fewer blocks per GiB, and a larger over-read. |

Remanence writes tapes only at these three block sizes (REM-PARITY §8.4). REM-OBJECT itself
allows any positive multiple of 512 bytes, and the drive accepts larger blocks (section 11), but
a Remanence tape uses one of the three. A range that straddles a block boundary reads two blocks
and discards less than `2 * chunk_size` in total.

The default is 256 KiB. Smaller blocks mean more blocks for the same data, and larger blocks mean
more over-read on every small range. For most PFR work, such as frame ranges of a video or
slices of a scientific dataset, an over-read of a few hundred KiB is small next to the tens of
seconds a LOCATE costs. The object's `chunk_size` is recorded with the object, so readers never
assume it. On tape it equals the block size of the object's tape file.

## 6. Compression

Compression can happen in the drive. REM-OBJECT itself does not compress.

### 6.1 LTO hardware compression

LTO drives compress data before writing it, with LTO Data Compression (LTO-DC). It can switch
between compressed and uncompressed encoding within the data stream, so data that does not
compress, such as random or encrypted data, does not expand (IBM LTO SCSI Reference, section
2.1.148). Compression is controlled by the DCE bit of the Data Compression mode page, page
`0Fh`, which Remanence sets with MODE SELECT. Compression changes how many physical bytes reach
the tape, but not the LBAs: if the application writes LBA 42 and reads LBA 42 back, it gets the
same bytes.

### 6.2 Format-level compression

There is none. REM-OBJECT performs no compression, because its payloads are mostly
already-compressed media, and compressing the stream would make a file's offset depend on
decompressing everything before it. That would remove the closed-form mapping of section 4.

### 6.3 Posture

Remanence turns hardware compression off whenever it configures a drive to write, and checks
with MODE SENSE that it is off before the first write. REM-PARITY requires it: its damage
guarantees count damaged blocks on the assumption that one logical block occupies one block's
worth of tape, which compression would break, so a parity tape whose bootstrap records drive
compression is refused (REM-PARITY §3.2, §8.2). Compression would also gain nothing on
already-compressed media.

## 7. Encryption

LTO drives can encrypt in hardware with AES256-GCM, set up through the SECURITY PROTOCOL IN
and SECURITY PROTOCOL OUT commands (LTO Program, data encryption; IBM LTO SCSI Reference,
section 6.8). Like hardware compression, hardware encryption leaves LBAs unchanged: LBA N reads
back the plaintext that was written at LBA N, and the drive encrypts and decrypts internally. So
with hardware encryption, block N can be read without reading block N-1.

Encrypted REM-OBJECT copies made with REM-ENCRYPT are a separate mechanism, applied in software
before the bytes reach the drive. Their stored chunks are mapped from the plaintext byte range
as section 4.6 and REM-ENCRYPT §6 define. [Encryption explained](encryption-explained.md)
compares the two.

<!-- code-anchor: crates/remanence-order crates/remanence-scsi/src/read_end_of_wrap_position.rs crates/remanence-api/src/read_plan.rs crates/remanence-api/src/calibration.rs crates/remanence-api/src/read_core.rs crates/remanence-library/src/handle/tape_io/mod.rs @ 311cc6c3 -->
## 8. Read ordering and batch restore

Ordering is not part of PFR itself; it speeds up restores of many targets. It matters here
because PFR turns a restore of a few large objects into many small scattered ranges, and that
is where ordering begins to pay. CERN reports that ordering recalls in software "can divide
required access time by up to two" (CERN-IT-Note-2020-001).

### 8.1 What the drive feature does

LTO-9 and later full-height drives offer Recommended Access Order (RAO). The application sends
the drive a list of the segments it wants, and the drive returns the same list sorted to reduce
the time spent positioning (IBM LTO SCSI Reference, section 4.6).

Each segment is a User Data Segment, described by a partition number, a beginning logical
object identifier and an ending logical object identifier. Both ends are needed, and the reason
is visible in what the drive returns: its estimated locate time for each segment is measured from
the end of the previous segment to the beginning of this one (section 5.2.24.1.3). The cost of
reading B after A depends on where A finishes, because that is where the head is left. An LTO
tape is written in wraps, passes along the tape that alternate in direction, so a long segment
may finish on a different wrap, travelling the other way.

### 8.2 Why Remanence computes the order itself

RAO is available only on LTO-9 and later full-height drives. It is not available on LTO-9
half-height drives, nor on LTO-8 or earlier. Remanence supports drives without it, so it
computes the order itself, from the cartridge's real wrap boundaries and the block ranges the
caller supplies.

The boundaries come from READ END OF WRAP POSITION (`A3h/1Fh/45h`, in its long form), which
reports the logical position at the end of every wrap that holds user data (IBM LTO SCSI
Reference, section 5.2.20). It is a different command from RAO, and drives support it from LTO-7
onward (IBM LTO SCSI Reference, table A.1). Remanence implements it in `remanence-scsi` and
records the result for each cartridge as a [wrap map](reference-glossary.md#wrap-map).

The timing is set by the drive, not chosen. IBM's reference says that the command's "return data
is valid at load" and that it "becomes stale on any write operation", and nothing in it says that
issuing the command again refreshes the data. Remanence therefore reads the wrap boundaries once
per load, after the load and before any write. A cartridge written during a load stays without a
valid wrap map until its next load, so a restore from a cartridge in the same load it was written
in gets no ordering benefit. Losing the stored map costs one command at the next load, because
the map can always be read again from the drive.

Remanence reads the boundaries from the drive instead of deriving them from each generation's
published capacity, because published capacity is not exact. IBM gives two figures for LTO-8,
12,000 GB and 11,600 GB, and tells system designers to read the application design capacity from
the drive at run time (IBM LTO SCSI Reference, section 3.4, table 5). The resulting error is not
proportional. Because wraps alternate in direction, a block near the end of one wrap is
physically close to the start of the next; an error large enough to shift a block's wrap by one
places it at the opposite end of the cartridge. An ordering built on approximate boundaries can
be no better than ascending block order, or worse than not ordering at all.

Ordering is available for LTO-7, LTO-8, LTO-9 and LTO-10 cartridges. LTO-7 Type M (M8)
cartridges and LTO-6 and earlier are reported as unsupported, because ordering on approximate
geometry can be worse than not ordering. Cartridges written with drive compression enabled are
also excluded, because the number of blocks in a wrap then depends on how well each wrap's
contents compressed, which cannot be computed from anything recorded.

The ordering is offered to clients as advice. The wrap-order computation and its cost model are
the `remanence-order` crate, which has no dependencies and performs no input or output. The daemon
answers the gRPC call `ReadPlanService.PlanBatchRead` from the stored wrap map, without touching
media and without committing anything (see the
[architecture overview](architecture-overview.md#read-order-planning)). No command in `rem`
issues this call; it exists for an external orchestrator. Planning an order and executing a batch
of reads are different things, and only planning exists (section 8.4).

### 8.3 The read contract

Reads for a drive are handled by the task that owns that drive, one at a time, and the read path
does not reorder them. Each `ReadObjectRange` and `ReadFile` call reads one target. Concurrent
calls from different clients are served one at a time, and the order between them is not
specified.

A plan is advice. The client asks Remanence for a good order and then issues its own reads in
whatever order it chooses. Reads address data by object and byte range, never by block number, so
a plan that has gone stale is slower than it could be but never incorrect, and no session or
validity window attaches to it.

### 8.4 Batched execution is not implemented

Remanence has no call that accepts a batch of read targets in one session and executes them
together. `PlanBatchRead` (section 8.2) only plans an order, and never reads.

## 9. Prior art

### 9.1 LTFS extent lists

LTFS, the Linear Tape File System, which Remanence does not implement, solves a similar mapping
problem with a list of extents for each file. Each extent has five fields (SNIA LTFS Format
Specification 2.5):

| Field | Meaning |
| --- | --- |
| `partition` | The partition that holds the extent. |
| `startblock` | The first logical block of the extent. |
| `byteoffset` | The offset into the start block at which the extent's data begins. |
| `bytecount` | The number of bytes in the extent. |
| `fileoffset` | The offset in the file at which the extent begins. |

A file can have several extents, and together they map any byte of the file to one position on
tape.

REM-OBJECT uses a simpler per-file index for three reasons. The main reason is that LTFS lets
several files share one tape block, which is what the `byteoffset` field is for (the LTFS
specification's shared-blocks example), while REM-OBJECT starts every file's payload on a block
boundary. That removes the offset within a block, at the cost of some unused space in the last
block of each file. A second reason is that an LTFS file can be split into several extents, while
a REM-OBJECT file is one contiguous run of blocks, so its index is a first block and a size, not
a list. A third is that an archive object is written once and never modified, so it does not need
the flexibility that extents give a file system.

### 9.2 A tar archive with a separate index

The ratarmount tool builds a SQLite index of a tar archive, by default in a file named
`<archive>.index.sqlite` beside it, that maps each member's path to its byte offset in the
archive. With the index, members can be opened and read at any offset without scanning the
archive. That works on a file or a disk. A tape needs more: the reader must know which tape block
holds a given byte, and what to discard after reading it.

A REM-OBJECT object is a plain pax tar stream, so standard `tar` reads it. It also carries its own
index and alignment. The manifest records each file's first block and size, and every file's
payload starts on a block boundary, so a byte range maps to whole tape blocks by the arithmetic of
section 4, without an external index file.

### 9.3 Broadcast archive products

Media archive products offer partial file restore by timecode. Drastic's Net-X-Copy indexes the
original files and creates a timecode-accurate proxy, after which "any section of the original
file can be recalled via time code or absolute offsets". Quantum's Timecode-Based Partial File
Retrieval generates an index for each new media file, "mapping timecode values in the original
media file to byte offsets". The tape mechanics underneath are the same as on this page: an index
built when the file is archived, a positioning command to the right block, a read of the covering
blocks, and a trim.

## 10. Summary: how REM-OBJECT supports PFR

1. One chunk per tape block in a plaintext copy. Every body block is `chunk_size` bytes and one
   tape block, so a range read positions once and then reads consecutive blocks. An encrypted copy
   stores chunks of `C + 16` bytes and uses the mapping in section 4.6.
2. `chunk_size` is recorded with the object. The default is 256 KiB. Readers take the value from
   the catalog, the bootstrap or the object's own header, never from an assumption.
3. Payloads start on block boundaries, so the position of any byte of a file follows from
   arithmetic.
4. There is a per-file index. The manifest and the catalog record `first_chunk_lba` and
   `size_bytes` for every file, and the same values serve plaintext and encrypted copies of an
   object.
5. There is no format-level compression, and Remanence turns hardware compression off.
6. Hardware encryption leaves block addressing unchanged, and REM-ENCRYPT maps plaintext ranges to
   stored ranges for software-encrypted copies (section 4.6).

## 11. Maximum block size

A drive reports its maximum block size in the READ BLOCK LIMITS response, not through MODE SENSE.
IBM's reference gives two different values. The reported `MAXIMUM BLOCK LENGTH LIMIT` field is
`80_0000h` (8 MiB), which IBM's note 15 explains is the maximum when the cartridge holds at least
one encrypted block. Without encrypted blocks the drive accepts blocks up to `FF_FFFFh` bytes
(about 16 MiB), but does not report that value, and IBM recommends staying within the reported
maximum (IBM LTO SCSI Reference, section 5.2.17.1, table 78 and note 15; section 4.11). When
Remanence reads a drive's configuration it issues READ BLOCK LIMITS and returns the reported
maximum with the rest of the configuration, as `TapeConfig::max_block_size_bytes`. Callers should
use that value instead of a fixed limit. Remanence's own blocks, at most 1 MiB, are well below
both values.

## Sources

- IBM. *IBM LTO SCSI Reference*, GA32-0928-08, 16 February 2026. Published through
  [IBM Support](https://www.ibm.com/support/pages/node/653373). Section numbers on this page refer
  to this edition.
- IBM. *Performance specifications for LTO tape drives* (TS4500 Tape Library documentation).
  <https://www.ibm.com/docs/en/ts4500-tape-library?topic=performance-lto-specifications>
- HP. *HP LTO Ultrium 6 Tape Drives Technical Reference Manual, Volume 4: Specifications*, first
  edition, September 2012. <https://docs.oracle.com/cd/E38452_01/en/LTO6_Vol4_E1/LTO6_Vol4_E1.pdf>
- LTO Program. *Frequently Asked Questions about LTO* (media optimization).
  <https://www.lto.org/faqs-about-lto/>
- LTO Program. *What is Data Encryption?* <https://www.lto.org/encryption/>
- Fujifilm. *LTO Ultrium data cartridge specifications*.
  <https://www.fujifilm.com/us/en/business/data-storage/data-storage-media/ltotape/specifications>
- T10. *SCSI Stream Commands - 5 (SSC-5)*. <https://www.t10.org/drafts.htm>
- CERN. *CERN-IT-Note-2020-001*. <https://repository.cern/records/7wtkv-e2e13>
- SNIA. *Linear Tape File System (LTFS) Format Specification*, version 2.5.
  <https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-v2.5-Technical-Position.pdf>
- ratarmount. <https://github.com/mxmlnkn/ratarmount>
- Drastic Technologies. *Net-X-Copy - Partial File Restore*.
  <https://www.drastic.tv/support-59/supporttipstechnical/44-net-x-copy-partial-file-restore>
- Quantum. *Timecode-Based Partial File Retrieval 2.0 User's Guide*, 6-67544-03 Rev. B, April 2021.
  <https://qsupport.quantum.com/kb/flare/Content/stornext/SN6_PDFs/Partial_File_Retrieval_Users_Guide.pdf>
