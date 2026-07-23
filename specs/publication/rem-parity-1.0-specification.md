# Rem Tape Parity (REM-PARITY) Format

## Version 1.0 — Specification

| | |
| --- | --- |
| Status | Draft for review |
| Version | 1.0 |
| Date | 2026-07-23 (v1.3.0 hardening increment) |
| License | CC-BY-4.0 |
| Concept DOI (all versions) | [10.5281/zenodo.21425126](https://doi.org/10.5281/zenodo.21425126) |
| Version DOI (this release) | assigned at release (the concept DOI above always resolves to the latest version) |
| Bootstrap magic | `52 45 4D 00 42 4F 4F 01` (`"REM\0BOO\x01"`, fixed bytes) |
| Erasure scheme identifier | `rs-cauchy-gf256-v1` |
| parity_map format identifier | `rem-parity-map-v1` |

## Status of This Document

This document is a draft specification, published for review. It is the
normative fixed point for the format it defines: an implementation is
validated against this document, not the reverse. The arithmetic test vectors
in this document (CRC, Reed–Solomon, canonical digest) are normative now and
independently re-derivable from this document alone; the image-level vectors
of Section 17 are *pinned-at-generation*, and producing them is a criterion
for declaring this specification final (Section 18). Specification-level
items still open before that point are collected in Appendix C. After freeze,
no normative change is permitted other than errata that do not change the set
of valid tapes; any other change produces a new major version.

## Abstract

This document specifies the Rem Tape Parity (REM-PARITY) format, version 1.0:
a self-describing, parity-protected method of writing multiple archival
objects to linear tape. Objects are opaque byte strings — this format never
interprets their content — written one per filemark-delimited tape file and
protected collectively: a fixed **bootstrap block** written at the beginning
of tape and at writer-chosen positions makes a bare tape self-describing;
**parity sidecar tape files** carry Reed–Solomon parity and per-block CRCs
for each parity epoch; an optional **parity_map tape file** carries the
sidecar epoch directory when it outgrows the bootstrap; and a **canonical
filemark-map digest** chains them together. Parity is computed over GF(2⁸)
with a Cauchy generator (`rs-cauchy-gf256-v1`), incrementally accumulated as
data streams, and written strictly in separate tape files, so every object
remains a clean, contiguous run of blocks readable with standard positioning
tools. The format is designed for catalog-less recovery: a reader holding
only this document, a damaged tape, and a generic SHA-256/HMAC/CBOR toolkit
can reconstruct the tape's structure, verify it cryptographically, and
recover up to *m* damaged blocks per stripe — including the case where the
damaged block is the first block of the very file that describes it.

## Table of Contents

1. [Introduction](#1-introduction)
2. [Conventions and Terminology](#2-conventions-and-terminology)
3. [Tape Model and Address Spaces](#3-tape-model-and-address-spaces)
4. [The Object Contract](#4-the-object-contract)
5. [Common Primitives](#5-common-primitives)
6. [The Erasure Scheme rs-cauchy-gf256-v1](#6-the-erasure-scheme-rs-cauchy-gf256-v1)
7. [The Filemark Map and the Canonical Digest](#7-the-filemark-map-and-the-canonical-digest)
8. [The Bootstrap Block](#8-the-bootstrap-block)
9. [The Parity Sidecar Tape File](#9-the-parity-sidecar-tape-file)
10. [The parity_map Tape File and the Sidecar Epoch Directory](#10-the-parity_map-tape-file-and-the-sidecar-epoch-directory)
11. [Writer Obligations](#11-writer-obligations)
12. [Scanner Obligations](#12-scanner-obligations)
13. [Recoverer Obligations](#13-recoverer-obligations)
14. [Resumer Obligations](#14-resumer-obligations)
15. [Errors](#15-errors)
16. [Security Considerations](#16-security-considerations)
17. [Test Vectors](#17-test-vectors)
18. [Conformance and Freeze Criteria](#18-conformance-and-freeze-criteria)
19. [IANA Considerations](#19-iana-considerations)
20. [References](#20-references)

Appendix A. [Worked Examples (Informative)](#appendix-a-worked-examples-informative)
Appendix B. [Design Rationale (Informative)](#appendix-b-design-rationale-informative)
Appendix C. [Open Items Before Freeze (Informative)](#appendix-c-open-items-before-freeze-informative)

---

## 1. Introduction

### 1.1. Purpose and Design Goals

REM-PARITY defines how a sequence of archival objects is laid out on one
linear tape and how that layout survives media damage. Its design goals, in
priority order:

1. **Catalog-less recovery.** Everything needed to map, verify, and repair a
   tape lives on the tape. Off-tape state (catalog, journal) accelerates
   recovery but is never required for it.
2. **Payload independence.** Objects are opaque byte strings. The format
   never reads object content to classify, map, verify, or repair a tape;
   any archiving tool whose objects meet the Section 4 contract can write
   and recover conformant tapes.
3. **Clean coexistence with standard tooling.** Parity and self-description
   live in separate, filemark-delimited tape files. An object occupies its
   own tape file as a contiguous run of fixed blocks, navigable with
   standard positioning tools (`mt fsf` + read); no parity byte ever appears
   inside an object.
4. **Bounded damage tolerance with bounded memory.** The default geometry
   tolerates a contiguous burst of up to `S × m` blocks (~512 MiB) per epoch —
   **including bursts that straddle the data→sidecar boundary** (Section 9.1,
   Appendix B.2) — while the writer holds only `S × m` parity accumulators
   (~512 MiB at the default geometry), regardless of object sizes. A short final
   or checkpoint epoch that closes fewer than `S` real data blocks (< 128 MiB at
   the default geometry) has a reduced boundary tolerance of `≈ (m − 1) × S`
   (~384 MiB), because its data shards sit adjacent to the full parity region
   (Appendix B.2).
5. **No circular failure.** The structures that describe the tape are
   replicated and discoverable such that single-block damage to any one of
   them — including the first block of a tape file — never makes an
   unrelated epoch unrecoverable (Sections 12.4, 12.5, 13.3).
6. **Fail-closed durability.** A tape file either completed its blocks, its
   trailing filemark synchronized to medium, and its durable off-tape commit
   record, or it does not exist for recovery purposes (Sections 3.4, 11.1).
7. **Long-term recoverability.** A future implementer holding only this
   document and its static test vectors can read every conformant tape;
   every cryptographic and arithmetic primitive is fully parameterized here.

### 1.2. One Tape, Many Objects

A REM-PARITY tape is a sequence of filemark-delimited tape files:

```text
| bootstrap(0) | object | object | sidecar | object | ... | bootstrap(final) | EOD
```

A Writer appends objects one per tape file. As object blocks stream to tape,
the Writer accumulates Reed–Solomon parity over them in fixed-size **stripes**
grouped into **epochs**; each completed epoch's parity and per-block CRCs are
written as a **parity sidecar** tape file at the next object boundary. Epochs
span objects: the parity geometry is independent of object sizes, and an
object needs no minimum size to be protected. A **bootstrap block** — tape
file 0, and again at writer-chosen checkpoints and at finish — records the
tape's identity, its parity scheme, a cryptographic digest of the tape's
structural table of contents (the **filemark map**), and a directory of the
sidecars. A bare, damaged tape is recovered by finding any bootstrap,
reconstructing and validating the filemark map, and repairing damaged blocks
from parity — all without reading a single byte of object content.

### 1.3. Relationship to Adjacent Components

- **Object formats above** (e.g. [RAO]) define the bytes inside object tape
  files; this format treats them as opaque fixed blocks (Section 4).
- **The tape I/O layer below** provides fixed-block reads and writes,
  filemarks, positioning, and boundary classification; Section 3.5 states
  what this format requires of it.
- **The commit store and catalog beside** are local, off-tape records. Their
  formats are out of scope; Section 3.4 defines the abstract *commit record*
  they implement, and Section 14 defines how a Resumer uses the committed
  prefix they describe.
- Drive hardware compression is required to be off for parity-protected
  tapes (Section 11.4): block bytes must map 1:1 to media so damage
  geometry and parity coverage correspond.

### 1.4. Non-Goals

This format performs no encryption and no authentication (Section 16.1) —
confidentiality and authenticity of object content belong to the object
format. It does not define capacity or placement *policy* (only the policy's
wire consequences), does not define the commit-store, journal, audit, or
catalog formats, and does not support multiple tape partitions (all positions
are partition 0).

## 2. Conventions and Terminology

### 2.1. Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174]
when, and only when, they appear in all capitals, as shown here.

### 2.2. Conformance Roles

A single implementation may fill several roles.

- **Writer**: produces parity-protected tapes (Section 11).
- **Scanner**: reconstructs the filemark map from a bare tape (Section 12).
- **Recoverer**: reconstructs damaged data blocks from parity (Section 13).
- **Resumer**: re-opens a committed tape for append (Section 14).
- **Verifier**: validates a tape's structures and digests end to end without
  recovering payload — the Scanner's checks plus the Recoverer's index and
  CRC validation, reporting all nonconformities rather than stopping at the
  first.

### 2.3. Definitions

- **Block / block size**: the tape's fixed block size; one value per tape,
  recorded in the bootstrap. All structures in this document are sized in
  blocks of this one size. All tape I/O is in whole blocks.
- **Tape file**: a run of one or more fixed blocks delimited by exactly one
  trailing filemark. Kinds: **object**, **parity sidecar**, **bootstrap**,
  **parity_map**.
- **Object**: one opaque archival byte string occupying exactly one object
  tape file (Section 4).
- **Stripe**: one Reed–Solomon codeword: `k` data shards plus `m` parity
  shards, each shard one full block.
- **Epoch**: one parity protection unit covering a non-empty explicit,
  half-open range of at most `S × k` data ordinals. Epoch ids are bare
  monotonic counters; they do not encode an ordinal or range.
- **Shard**: one block in its role as a stripe member (data or parity).
- **`ParityDataOrdinal` (ordinal)**: the dense index of an object data block
  in the tape's protection stream (Section 3.2).
- **Watermark `W`** (`highest_protected_ordinal`): ordinals `< W` are
  covered by emitted sidecars. **Total `T`** (`map_total_data_ordinals`):
  ordinals `< T` exist as committed data. Always `W ≤ T`.
- **Committed**: inside the durable boundary (Section 3.4).
- **Synchronizing barrier**: a tape I/O operation whose successful
  completion proves every previously issued block and filemark of the
  session is on medium (Section 3.5).
- **Attested prefix**: the leading tape files covered by the
  highest-authority validating digest record (Section 12.6).
- **Filemark map**: the tape's structural table of contents — one entry per
  tape file (Section 7).
- **Sidecar epoch directory**: the per-epoch repair root listing every
  sidecar's location, range, layout counts, and metadata hash (Section 10).
- **Implicit zero**: a logical data position beyond a short epoch's real
  data — an all-zero shard that is never written to tape
  (Section 6.4).

### 2.4. Integer, Byte, and Text Conventions

All multi-byte integers in sidecar and parity_map structures are
**little-endian**. The bootstrap header mixes endianness per field — its
table (Section 8.1) is authoritative, and the mix is deliberate and frozen
(Appendix B.5). All offsets are zero-based. `KiB` = 2^10 bytes; `MiB` = 2^20
bytes. Hexadecimal values are prefixed `0x`. Ranges written `a..b` — byte ranges
and the index ranges of Section 6 alike — are half-open (end-exclusive), so
`0..n` has exactly `n` elements. LBA denotes a logical block address; EOM denotes
end of medium. SHA-256 is the hash function of [FIPS180-4]. Text fields
(scheme and format
identifiers, version strings, timestamps) are UTF-8 [RFC3629]. Arithmetic on values
read from tape MUST be checked; overflow is rejection, never wraparound
(Section 16.2).

### 2.5. Constants

| Constant | Value |
| --- | --- |
| `BOOTSTRAP_MAGIC` | `52 45 4D 00 42 4F 4F 01` (`"REM\0BOO\x01"`, fixed bytes) |
| `BOOTSTRAP_SCHEMA_MAJOR` / `MINOR` | 1 / 2 (minor 2 added RAO object rows, key 30; Section 8.2.1) |
| `BOOTSTRAP_HEADER_LEN` | 0x34 |
| `FLAG_NO_PARITY` | bit 0 of the bootstrap flags |
| `MAX_BOOTSTRAP_SCAN_BLOCKS` | 1024 |
| Block-size discovery candidates | 256 KiB, 512 KiB, 1 MiB (Section 8.4) |
| `SIDECAR_MAGIC_LABEL` | `"REM\0PAR\x01"` (8 bytes: `52 45 4D 00 50 41 52 01`) |
| `SIDECAR_FOOTER_MAGIC_LABEL` | `"REM\0PARFOOT\x01"` (12 bytes: `52 45 4D 00 50 41 52 46 4F 4F 54 01`) |
| `PARITY_MAP_MAGIC_LABEL` | `"REM\0PMAP\x01"` (9 bytes: `52 45 4D 00 50 4D 41 50 01`) |
| `PARITY_MAP_FOOTER_MAGIC_LABEL` | `"REM\0PMAPFOOT\x01"` (13 bytes: `52 45 4D 00 50 4D 41 50 46 4F 4F 54 01`) |
| `SIDECAR_SCHEMA_VERSION` | 1 |
| `SIDECAR_HEADER_LEN` / header CRC offset | 0xB8 / 0xB0 |
| `SIDECAR_FOOTER_LEN` / footer CRC offset | 0x80 / 0x78 |
| `PARITY_INDEX_ENTRY_LEN` | 16 |
| `DATA_CRC_ENTRY_LEN` | 8 |
| `PARITY_MAP_FORMAT_ID` | `"rem-parity-map-v1"` |
| `PARITY_MAP_SCHEMA_VERSION` / footer version | 1 / 1 |
| `PARITY_MAP_HEADER_LEN` = footer len / CRC offsets | 0xB8 / 0xB0 |
| `SCHEME_ID` | `"rs-cauchy-gf256-v1"` |
| Default scheme | k = 128, m = 4, S = 512 at 256 KiB blocks (Section 6.6) |
| `SIDECAR_METADATA_HASH_DOMAIN` | `"remanence-sidecar-metadata-v1"` (29 ASCII bytes) |
| Tape-file kind codes | Object = 0, ParitySidecar = 1, Bootstrap = 2, ParityMap = 3 |
| Minimum block sizes | bootstrap frame 0x3C; parity_map 0xB8; sidecar 0xC0 |

## 3. Tape Model and Address Spaces

### 3.1. Tape Files

A REM-PARITY tape is a sequence of tape files numbered densely from 0 at the
beginning of tape (BOT), each terminated by exactly one filemark, followed by
end of data (EOD):

```text
| bootstrap(0) | object | object | sidecar | object | ... | bootstrap(final) | EOD
```

Tape file 0 MUST be a bootstrap. This format owns every filemark on the
tape: an object's bytes MUST NOT depend on filemarks for internal structure,
and a Writer MUST NOT emit filemarks except as tape-file terminators. A tape
file MUST contain at least one block — an immediate filemark is structural
damage.

### 3.2. Address Spaces

- **`TapeFilePosition`** = `(tape_file_number: u32, block_within_file: u64)`.
- **Logical LBA** (partition 0): the SCSI logical block/object position — *not* a
  physical media address — where each prior tape file contributes its blocks plus
  one filemark, so
  `LBA(f, b) = Σ_{g<f}(block_count(g) + 1) + b`. The append point after a
  committed prefix is `Σ(block_count + 1)` over all committed files. All damage
  guarantees in this format are expressed in **logical block erasures** at this
  position space; their mapping to physical media damage holds only under the
  block-to-media identity of Section 16.3 (one logical block ⇔ one block's worth
  of media), which is why drive compression is rejected (Sections 8.4, 11.4).
- **`ParityDataOrdinal`** (u64): the dense numbering of object data blocks
  only, in tape order, skipping filemarks and all non-object tape files. For
  an object tape file whose first block has ordinal `F`, block `b` of the
  file has `ordinal(b) = F + b`. Non-object files have no ordinals; parity
  shards have no ordinals. First-ordinals are dense and contiguous: in tape
  order, each object file's first ordinal equals the previous running total
  of object blocks, starting at 0.
- **Object block index**: the zero-based block index within one object's
  tape file — the address an object format uses internally.
  `(tape_file_number, object_block_index)` resolves to an ordinal through
  the filemark map (Section 7).

### 3.3. Ordinal-to-Stripe Mapping

For scheme `(k, m, S)` (Section 6.6), locate the unique sidecar descriptor
whose explicit range `[start, end)` contains ordinal `o`. Let `E = S × k`
and `d = o − start`; then:

```text
epoch       = descriptor.epoch_id
o_in_epoch  = d
stripe      = d mod S                  (the stripe index varies fastest)
data_index  = d / S                    (0 ≤ data_index < k)

inverse:  o = start + data_index·S + stripe
```

The interleave is essential: `N ≤ S` physically consecutive data blocks
land in `N` distinct stripes, so contiguous damage of up to `S × m` blocks
stays within the per-stripe tolerance `m` (Appendix B.2). Parity shards are
addressed `(epoch, stripe, parity_index)` with `0 ≤ parity_index < m`; they
live in sidecar tape files (Section 9), never among the data blocks, and are
stored **parity-index-major** (Section 9.1) so the same interleave extends into
the parity region: consecutive parity blocks belong to consecutive stripes. A
contiguous burst that straddles the data→sidecar boundary therefore spreads
across stripes on both sides, and the guarantee holds across the boundary, not
only within object data (Appendix B.2).

### 3.4. The Durable Boundary

A tape file is **committed** only when all of the following hold, in
order: its blocks and its trailing filemark are written; blocks and filemark
are **synchronized to medium** — by a synchronous filemark, or by a later
synchronizing barrier (Section 11.1) completing before the commit record;
and a durable off-tape **commit record** exists. The commit record's format is implementation-defined (a
journal, a database row, a replicated log entry); its required content is
the tape file's filemark-map entry (Section 7.1) plus enough state to seed a
Resumer (Section 14), and it MUST record both. There is no on-tape commit
marker and no on-tape
"unclean" marker: an interrupted tail simply lies beyond the last committed
file and is physically superseded on resume (Section 14). Tape files are
written and numbered strictly sequentially (next = last committed + 1,
first = 0, at most one in flight); the commit records of an object and of
the sidecars emitted at its close MAY be folded into one durable transaction
(Section 11.1). Readers seeded from a *prefix*-scoped map
(Section 7.4) MUST treat rows beyond the validated prefix as forensic only —
never recovery inputs.

Checkpoint bootstraps (Section 8.3) give the committed prefix an on-tape
counterpart at barrier grain. Because the tape I/O layer persists writes in
submission order (Section 3.5), a bootstrap block readable at tape file `F`
implies every block and filemark of files `0..F−1`, and the blocks of file
`F`, were written to the medium; when that bootstrap's digest record
validates (Section 12.4), its scope is additionally structurally proven.
Section 12.6 defines what a reader holding only the cartridge may conclude
from this. Attestation is not commitment: the off-tape record remains the
Writer's sole commit acknowledgment, and where off-tape commit records are
available the committed prefix governs recovery (Sections 13.2, 14) — the
attested prefix is the cartridge-alone fallback.

### 3.5. Requirements on the Tape I/O Layer

Fixed-block reads and writes only; a read returning other than exactly one
block is an error, with two classified boundary outcomes: **Filemark** and
**EndOfData**. For transports reporting SCSI sense data, boundary
classification MUST work for both fixed-format and descriptor-format sense
data [LTO-SCSI]; other transports MUST provide equivalent Filemark and
EndOfData classification. Implementations MAY track position by
+1-per-block dead reckoning but MUST resynchronize via a positional query
(e.g. SCSI READ POSITION) after any boundary or unclassified error.

The tape I/O layer MUST persist blocks and filemarks to medium strictly in
submission order, and MUST provide a **synchronizing barrier**: an
operation (e.g. a zero-count synchronous SCSI WRITE FILEMARKS) whose
successful completion proves that every block and filemark issued before it
is on medium. A completion-unknown outcome at a barrier MUST be treated as
barrier failure. On a transport that cannot guarantee ordered persistence,
the attestation conclusions of Sections 3.4 and 12.6 are void.

## 4. The Object Contract

### 4.1. Objects Are Opaque Block Strings

An **object** is a byte string whose length is a positive exact multiple of
the tape block size. It is written as exactly one object tape file of
`block_count = length / block_size` contiguous blocks followed by one
filemark. This format places no constraint on the bytes themselves: any
block content is valid, and nothing in this format ever parses object bytes.
A Writer MUST refuse an object whose length is not a positive multiple of
the block size; padding an object up to a block multiple is the payload
format's (or the caller's) responsibility and becomes part of the object's
bytes, indistinguishable from content to this format.

### 4.2. What the Format Provides to Objects

For every **protected** object block — every block whose ordinal is below
the watermark `W`, i.e. whose epoch's sidecar has been emitted — this
format holds a CRC-64/XZ of the block in that sidecar (Section 9.3) and
protects the block with Reed–Solomon parity (Section 6) at the epoch's
geometry. Between sidecar emissions, fewer than one epoch's worth of
committed data is pending (`W ≤ ordinal < T`, Section 11.2): durable, but
not yet repairable, and recovery refuses it as such (Section 13.2).
`finish()` (Section 11.3) closes the final epoch, so on a finished tape every object
block is protected. The format provides:

1. **Structural addressing.** `(tape_file_number, object_block_index)`
   resolves through the filemark map to a `ParityDataOrdinal` and back, so
   damaged-block reports and recovered blocks are exchanged in either
   address space.
2. **Block-granular detection.** A block whose stored CRC mismatches is
   detected without any payload-format knowledge.
3. **Bounded repair.** Up to `m` damaged blocks per stripe are
   reconstructed from any `k` surviving shards (Section 13), keyless and
   content-blind — an encrypted object is repaired without its keys.
4. **Commit semantics.** An object is durable exactly when its tape file is
   committed (Section 3.4); the Writer reports completion only then.
5. **Catalog-less rediscovery.** A Scanner classifies object tape files by
   elimination — never by reading object content (Section 12.3) — so a tape
   of foreign objects is mappable by any conformant implementation.

This format provides integrity and recovery at **block** granularity only.
End-to-end content integrity (per-file digests, manifests) and
confidentiality are the payload format's job (Section 16.4).

### 4.3. What the Format Requires of Payload Formats

A payload format carried as REM-PARITY objects:

1. MUST produce objects whose stored length is a positive exact multiple of
   the tape block size (Section 4.1).
2. MUST NOT require filemarks, tape positioning side effects, or any medium
   feature inside an object: an object round-trips as a plain byte string
   through any storage that preserves bytes.
3. MUST tolerate that a reader is handed whole blocks: a payload format
   SHOULD be self-framing (carry its own end-of-content structure) so that
   its content length is recoverable from its own bytes.
4. SHOULD be self-describing and carry its own content-level integrity
   (per-file digests or equivalent), because this format verifies blocks,
   not meaning.
5. SHOULD make its objects identifiable from their own bytes (a magic, a
   header) if catalog-less payload recovery matters to it; this format's
   generic structures identify *which tape files are objects* but do not
   parse object bytes. Payload bindings MAY add bounded descriptive rows
   through the bootstrap extension surface (Section 8.2.1), with the
   leakage constraints stated in Section 16.4.

### 4.4. Payload Format Bindings (Informative)

**Rem Archive Objects [RAO].** An RAO object meets the contract by
construction: its stored bytes are an exact positive multiple of the
object's `chunk_size` in both representations (plaintext and encrypted);
with the tape block size equal to `chunk_size`, one RAO body block is one
tape block. Parity is computed over stored bytes — ciphertext when
the object is encrypted — so damaged encrypted objects are repaired keyless
and decryption is retried on the recovered bytes.

**Plain tar.** A POSIX tar archive zero-padded to a block multiple meets the
contract: it is self-framing (tar end-of-archive records), self-describing,
and recoverable from an unmapped tape with `mt fsf <n>` plus
`tar -b <block_size/512> -xf <device>` — while still enjoying block CRCs and
parity repair from this format.

## 5. Common Primitives

### 5.1. CRC-64/XZ

All CRCs in this format are CRC-64/XZ: polynomial `0x42F0E1EBA9EA3693`,
reflected input and output (reflected polynomial constant
`0xC96C5795D7870F42`), initial value `0xFFFF_FFFF_FFFF_FFFF`, final XOR
`0xFFFF_FFFF_FFFF_FFFF`. CRC values are stored little-endian. Normative
vectors:

```text
crc64("123456789")        = 0x995DC9BBDF1939FA   (LE bytes fa 39 19 df bb c9 5d 99)
crc64("")                 = 0
crc64([0x00])             = 0x1FADA17364673F59
crc64([0xFF])             = 0xFF00000000000000
crc64(0x00 × 262144)      = 0x261BDF3D299838FC
crc64(0xFF × 262144)      = 0x55433DD0F38908BA
```

### 5.2. HMAC-Derived Magics

Sidecar and parity_map blocks carry **per-tape** magics:

```text
magic = HMAC-SHA-256(key = tape_uuid[16 bytes], message = LABEL)[0..8]
```

where HMAC is [RFC2104] with SHA-256 [FIPS180-4], `tape_uuid` is the 16-byte
tape identity from the bootstrap, `LABEL` is the role's ASCII label from
Section 2.5 (label bytes exactly as listed, including the embedded NUL and
trailing 0x01; no terminator added), and `[0..8]` takes the first 8 bytes of
the 32-byte MAC. The label bytes never appear on tape. Each block role has a
distinct label (sidecar header vs footer; parity_map header vs footer). The
bootstrap magic alone is a fixed byte string, because the reader does not
yet know the tape UUID when it searches for a bootstrap (Section 8.1).
Derived magics are an *identity* mechanism — these blocks belong to this
tape and role — not authentication (Section 16.1).

### 5.3. Deterministic CBOR

All CBOR frame payloads in this format are single definite-length,
integer-keyed maps in RFC 8949 deterministic encoding: shortest-form
integers and lengths, map keys sorted in ascending order of their
deterministic encodings, no duplicate keys, no tags, no floats, no
indefinite-length items. The Section 7.3 canonical-digest preimage is not a
frame payload or a map: it applies these canonical-encoding rules to its
specified array-of-arrays structure. Each payload map MUST occupy its entire
declared payload extent (for the bootstrap, `cbor_payload_len`, Section 8.1);
bytes after the map's definite-length encoding, within the declared payload,
are a nonconformity. Decoders MUST reject duplicate keys and
non-canonical encoding, and MUST ignore unknown integer keys at every map
level — that is the format's 1.x extension mechanism: future minor revisions
add keys; they never change the meaning of existing ones (a change that
would, requires a new schema version or magic). Allocation while decoding
MUST be bounded by the physically measured byte length of the input, never
by counts read from the CBOR stream (Section 16.2).

## 6. The Erasure Scheme rs-cauchy-gf256-v1

### 6.1. The Field GF(2⁸)

The scheme operates over GF(2⁸) with reduction polynomial **0x11D**
(x⁸ + x⁴ + x³ + x² + 1). Field elements are bytes. Addition is XOR.
Multiplication `gf_mul(a, b)` is carry-less polynomial multiplication
reduced modulo 0x11D, computable bit-serially:

```text
gf_mul(a, b):
    p = 0
    repeat 8 times:
        if b & 1: p = p XOR a
        b = b >> 1
        a = a << 1
        if a & 0x100: a = a XOR 0x11D
    return p
```

Inversion is `inv(v) = v^254` (Fermat exponentiation in the 255-element
multiplicative group); `inv(0)` is an error. Implementations are free to use
lookup tables, log/antilog tables, or SIMD kernels, provided the results are
byte-identical to the definitions above (Section 18 criterion 6).

### 6.2. The Cauchy Generator

The generator is the `m × k` Cauchy matrix with the contiguous seed
partition:

```text
X_j = k + j   (j in 0..m)        Y_i = i   (i in 0..k)
G[j][i] = inv(X_j XOR Y_i)       requires k + m ≤ 255
```

The seed partition is fixed; the matrix is fully determined by `(k, m)` and
MUST be derived exactly as above.

### 6.3. Encoding

Encoding is systematic and byte-wise across full blocks:

```text
parity_j = XOR over i in 0..k of  G[j][i] ⊗ data_i
```

where `⊗` is GF(2⁸) scalar-by-block multiplication (each byte of the block
multiplied by the scalar) and `data_i` is the full data shard at stripe
position `i`. Encoding MUST be expressible as order-independent incremental
accumulation — `accumulate(i, shard)` XORs `G[j][i] ⊗ shard` into each of
`m` zero-initialized accumulators — and incremental and batch encodings MUST
be byte-identical. This is what lets a Writer stream object data without
buffering an epoch (Section 11.2).

### 6.4. Implicit Zeros

In a short epoch, logical data positions beyond the real data are all-zero
shards that are *never written to tape* and never accumulated (an all-zero
shard contributes nothing to any parity accumulator). The sidecar's
`real_data_shard_count` versus `logical_shard_count = S × k` tells readers
which positions are implicit (Section 9.2). Implicit-zero positions are
never erasures: a reader supplies an all-zero block for them during
reconstruction (Section 13.4).

### 6.5. Reconstruction

Given any `k` of the `k + m` shards of a stripe — data shards first in index
order, then parity shards — form the corresponding rows of the systematic
generator `[I_k ; G]`, invert that `k × k` matrix by Gauss–Jordan
elimination over GF(2⁸), and multiply to recover the missing data shards;
re-encode any missing parity from the recovered data. Fewer than `k`
survivors is unrecoverable. Maximum tolerated erasures per stripe: `m`.

### 6.6. Scheme Parameters and Profiles

A scheme is the triple `(k, m, S)` plus the scheme identifier. Validity:

```text
k ≥ 2      1 ≤ m ≤ k      S ≥ 1      k + m ≤ 255      S × (k + m) ≤ 2³² − 1
```

The scheme triple is recorded in the bootstrap and in every sidecar; readers
MUST use the recorded values, never defaults. The profiles below are
informative writer defaults, with `S` chosen as
`max(1, ceil(target / (block_size × m)))` for a contiguous-damage target:

| Profile | k | m | Damage target | At 256 KiB blocks | Parity overhead |
| --- | ---: | ---: | --- | --- | ---: |
| Default | 128 | 4 | 512 MiB | S = 512; epoch = 65,536 data + 2,048 parity blocks; tolerance 2,048 contiguous blocks | 3.125% |
| Conservative | 64 | 6 | 384 MiB | S = 256; tolerance 1,536 contiguous blocks | 9.375% |

### 6.7. Per-Shard CRCs

`data_shard_crc64` is the CRC-64/XZ over the entire fixed data block as
written through the parity path. `parity_shard_crc64` is the CRC-64/XZ over
the entire raw parity shard block. Both are recorded in the sidecar index
(Section 9.3); they are the verify-before-trust and
verify-after-reconstruction anchors (Section 13.4, 13.5).

### 6.8. Normative Vectors

```text
gf_inv(0x02) = 0x8E      gf_inv(0x03) = 0xF4

k = 2, m = 2  ⇒  G = [[0x8E, 0xF4],
                      [0xF4, 0x8E]]

data   d0 = 01 02 03 04      d1 = 10 20 30 40
parity p0 = 75 EA 9F C9      p1 = FC E5 19 D7
```

A conformant codec MUST reproduce these values and MUST pass full-stripe
reconstruction for every erasure pattern of up to `m` erasures at this
geometry (Section 17).

## 7. The Filemark Map and the Canonical Digest

### 7.1. Entries

The filemark map is the tape's structural table of contents — one entry per
tape file:

| Field | Type | Applies to |
| --- | --- | --- |
| `tape_file_number` | u32, dense from 0 | all |
| `kind` | Object = 0, ParitySidecar = 1, Bootstrap = 2, ParityMap = 3 | all |
| `block_count` | u64 ≠ 0 — data blocks, excluding the filemark; MUST be 1 for bootstraps | all |
| `first_parity_data_ordinal` | u64 | objects only |
| `protected_ordinal_start` / `protected_ordinal_end_exclusive` | u64, half-open range | sidecars only |
| `epoch_id` | u64 | sidecars only |

### 7.2. Validity

Tape file numbers are dense from 0. Object first-ordinals are dense and
contiguous from 0 in tape order (Section 3.2). Kind-specific fields are
exclusive to their kinds; an entry carrying a field outside its kind is
invalid. Derived scalars:

```text
T = max(first_parity_data_ordinal + block_count) over object entries    (0 if none)
W = max(protected_ordinal_end_exclusive)         over sidecar entries   (0 if none)
```

### 7.3. The Canonical Digest

The canonical digest is SHA-256 over the deterministic CBOR encoding
(Section 5.3 canonical-encoding rules, applied to this array structure rather
than to an integer-keyed map) of an array of per-entry 7-element arrays,
ascending by tape file number:

```text
[tape_file_number, kind_code, block_count,
 first_parity_data_ordinal        | null,
 protected_ordinal_start          | null,
 protected_ordinal_end_exclusive  | null,
 epoch_id                         | null]
```

Fields that do not apply to the entry's kind are CBOR `null` (0xF6).

**Exclusions — the non-circularity rule.** The digest covers no physical
position hints, no content hashes, no bootstrap or parity_map payload bytes,
and no copy-health state. This is what lets a bootstrap's digest cover the
map *including the bootstrap's own structural entry*: the digest of the
projected map is computed before the bootstrap is written, and writing the
bootstrap does not change the projection. It is also why discovering damage
never invalidates the map (Section 12.5).

**Normative vector.** The map
`[bootstrap(#0, 1 blk), object(#1, 3 blk, first ordinal 0), sidecar(#2,
2 blk, epoch 7, range [0, 3))]` projects to the 25 bytes

```text
83 87 00 02 01 f6 f6 f6 f6
   87 01 00 03 00 f6 f6 f6
   87 02 01 02 f6 00 03 07
```

with SHA-256

```text
548ca6c967073a6c1ad011d10fc132c2739e251d015ea45a628bbec96892c26b
```

(The byte-by-byte derivation is worked in Appendix A.3. The map is
deliberately synthetic: it exercises the digest encoding only and does not
describe a constructible tape — a real sidecar cannot occupy 2 blocks, and
epoch 7 could not protect ordinal range [0, 3).)

### 7.4. The Digest Record and Scope

Wherever it is stored (bootstrap key 2, parity_map payload and locators),
the digest travels with its scope:

| Field | Meaning |
| --- | --- |
| `sha256` | the canonical digest |
| `tape_file_count` | the prefix length (number of leading tape files) the digest covers |
| `map_total_data_ordinals` | `T` over that prefix |
| `highest_protected_ordinal` | `W` over that prefix |
| `is_final_map` | whether the prefix is the whole tape |

Validation MUST recompute the digest over exactly the leading
`tape_file_count` entries and cross-check all three scalars. A final digest
yields a **Complete** map; a non-final digest yields a **Prefix** map valid
for exactly `tape_file_count` files. Recovery MUST be fenced to the
validated scope (Section 13.2).

## 8. The Bootstrap Block

The bootstrap is the tape's UUID-independent entry point: a single block,
findable by magic, that records the tape's identity, block size, parity
scheme, and the digest record governing map validation. It is the only
structure in this format with a fixed (non-derived) magic, because the
reader does not yet know the tape UUID when searching for it.

### 8.1. Fixed Frame (one block, exactly)

A bootstrap tape file is exactly one block:

| Offset | Len | Field | Type | Constraint |
| --- | ---: | --- | --- | --- |
| 0x00 | 8 | magic | fixed bytes | `BOOTSTRAP_MAGIC` |
| 0x08 | 2 | schema_major | **u16 BE** | MUST be 1; readers reject ≠ 1 |
| 0x0A | 2 | schema_minor | **u16 BE** | readers accept any value |
| 0x0C | 4 | flags | **u32 BE** | bit 0 = no-parity; all other bits MUST be 0 |
| 0x10 | 16 | tape_uuid | raw bytes | the tape's identity (16 opaque bytes; RECOMMENDED a version-4 UUID [RFC9562], unique per tape); the HMAC key of Section 5.2 |
| 0x20 | 4 | block_size_bytes | **u32 BE** | MUST equal the size of the block it was read with |
| 0x24 | 4 | sequence | **u32 BE** | 0 at BOT; strictly increasing across all copies |
| 0x28 | 4 | cbor_payload_len | **u32 LE** | payload byte length |
| 0x2C | 8 | crc64_header | **u64 LE** | CRC-64/XZ over bytes 0x00..0x2C |
| 0x34 | var | CBOR payload | Section 8.2 | |
| +len | 8 | crc64_payload | **u64 LE** | CRC-64/XZ over the payload bytes |
| … | | zero fill to block end | | MUST be written zero; not an acceptance rule (see below) |

The endianness mix — big-endian header integers, little-endian length and
CRCs — is frozen verbatim; implementations MUST NOT "normalize" it
(Appendix B.5). The minimum viable block size is 0x3C (header plus the
payload CRC of an empty payload). Parse order: block length ≥ 0x3C → magic →
header CRC → schema_major → payload bounds (checked against the block) →
payload CRC → CBOR.

Writers MUST zero the trailing fill. Verifiers MUST verify it and report a
nonzero fill as a nonconformity, but bootstrap *acceptance* — during
discovery (Section 8.4) and classification (Section 12.3) — MUST NOT depend
on the fill, so a damaged fill byte cannot cost the tape its entry point.
This is the one deliberate exception to the Section 16.2 verify-zero rule.

### 8.2. CBOR Payload

A single integer-keyed map (Section 5.3):

| Key | Type | Presence | Meaning |
| ---: | --- | --- | --- |
| 1 | map | REQUIRED unless no-parity | scheme record: `{1: tstr scheme_id, 2: uint k, 3: uint m, 4: uint S}` |
| 2 | map | REQUIRED unless no-parity | digest record: `{1: bytes .size 32 sha256, 2: uint tape_file_count, 3: uint map_total_data_ordinals, 4: uint highest_protected_ordinal, 5: bool is_final_map}` — all five REQUIRED when the map is present |
| 3 | tstr | OPTIONAL | writer version (diagnostic) |
| 4 | tstr | OPTIONAL | [RFC3339] write timestamp |
| 5 | bool | REQUIRED (a Writer MUST write it); readers MUST treat absence as false | `drive_compression` — effective hardware compression at session open. `true` on a parity bootstrap MUST be rejected (Sections 8.4, 11.4) |
| 20 | map | OPTIONAL | inline sidecar epoch directory (Section 10.5) |
| 21 | map | OPTIONAL | `ParityMapReference` (Section 10.6) |
| 30 | array of maps | OPTIONAL | RAO object rows (Section 8.2.1) |

Keys 20 and 21 are mutually exclusive; both present is a parse error. A
**no-parity bootstrap** (flag bit 0 set) marks a tape written without parity
protection; it MAY omit everything except the fixed frame, and readers MUST
NOT require the scheme or digest records on it. For a parity bootstrap the
scheme record's `scheme_id` MUST be `rs-cauchy-gf256-v1` and `(k, m, S)`
MUST satisfy Section 6.6 validity. Unknown keys are ignored at every level
(Section 5.3). Writers SHOULD populate keys 3 and 4; absence is conformant.

### 8.2.1. RAO Object Rows

Key 30, when present, is an array of integer-keyed object row maps in
strictly increasing `tape_file_number` order:

| Row key | Type | Presence | Meaning |
| ---: | --- | --- | --- |
| 1 | uint | REQUIRED | filemark-delimited object `tape_file_number` |
| 2 | tstr | REQUIRED | representation marker: `"plaintext"` or `"encrypted"` |
| 3 | uint | REQUIRED | stored block count for the object tape file |
| 4 | bytes, 1–64 | REQUIRED from minor 3 | RAO `object_id` — the identity the archive answers "where is object X" with — carried verbatim as its 1–64 non-NUL bytes (any RAO envelope NUL padding of [RAO] §5.2 stripped). This matches [RAO] `object_id` exactly (opaque UTF-8, 1–64 bytes; capped in [RAO] §4.5.1) with no conversion. Readers of minors ≤ 2 tolerate its absence; a Writer at minor 3 or later MUST emit it. |
| 10 | uint | plaintext only | `manifest_first_chunk_lba` |
| 11 | uint | plaintext only | `manifest_size_bytes` |
| 12 | uint | plaintext only | `manifest_chunk_count` |
| 13 | bytes .size 32 | plaintext only | `manifest_sha256` |
| 21 | uint | encrypted only | RAO encrypted-header `metadata_frame_len`; bounds `[17, 16 MiB]` |
| 22 | array of bytes .size 16 | encrypted only | RAO key-frame `recipient_epoch_id` values; 1 through 8 distinct nonzero ids |
| 23 | uint | encrypted only | RAO encrypted-header `key_frame_len`; bounds `[103, 4096]` |

Key 10 (`manifest_first_chunk_lba`) is the zero-based block index, *within
the object's tape file*, of the first block of the manifest entry's payload —
an RAO inner `BodyLba`, not a Section 3.2 physical LBA; one RAO body block is
one tape block (Section 4.4). Key 11 is the manifest's payload byte length,
key 12 its block count, and key 13 the SHA-256 of its CBOR bytes. Key 21 is the
RAO encrypted-envelope header's `metadata_frame_len`; key 22 records the
recipient epoch ids present in its key frame; key 23 is the header's
`key_frame_len`. Their semantics and bounds are defined by [RAO], which is a
normative reference for implementations of key 30.

Plaintext rows MUST carry keys 10–13 and MUST NOT carry keys 21–23.
Encrypted rows MUST carry keys 21–23 and MUST NOT carry keys 10–13. For
plaintext rows, `manifest_chunk_count` and `manifest_size_bytes` MUST be
positive, the manifest chunk range MUST fit within `stored_block_count`, and
the manifest byte length MUST fit within `manifest_chunk_count ×
block_size_bytes`. For every row, `stored_block_count` MUST be positive and
MUST match the structural filemark-map row for key 1.

The row set is prefix-scoped exactly like the bootstrap digest. A checkpoint
bootstrap that carries key 30 MUST include one row for every object tape file
in that checkpoint's digest scope, and — for a Writer that implements RAO
object rows — the final bootstrap MUST include one
row for every committed object tape file in the final digest scope. A resumed
Writer MUST preserve rows for the committed prefix and append rows for new
objects; it MUST NOT emit a later authoritative bootstrap that silently drops
previously committed object rows.

Version 1.0 defines no external overflow carrier for key-30 rows. Therefore a
Writer that implements RAO object rows MUST perform admission control before
starting a new object and MUST reject the write if the resulting mandatory
row set could not fit in every subsequent bootstrap payload that must carry
it. The rejection happens before object bytes are written; failing only at
checkpoint or `finish()` time is nonconformant. A future minor version MAY
define an external carrier through an unknown bootstrap key, but readers MUST
NOT infer such a carrier in version 1.0.

A Writer that reaches this directory ceiling MUST seal at the last durable
checkpoint boundary and direct subsequent placement to another tape. It MUST
reserve worst-case row and checkpoint-stop headroom when opening a batch, so
the ceiling is an admission refusal between batches and never strands an open
batch after object bytes have moved.

### 8.3. Placement (Writer)

Two bootstraps are mandatory: sequence 0 at BOT (tape file 0), and a final
bootstrap whose digest record has `is_final_map = true` written at
`finish()` (Section 11.3). Between them, a Writer MAY write **checkpoint
bootstraps** (`is_final_map = false`, prefix-scoped digest) at positions
chosen by content-driven policy, evaluated only at object boundaries. The
placement policy and its parameters (byte or object-count floors,
end-of-medium taper, minimum physical separation) are writer-defined and not
normative; this format constrains only their observable consequences:
checkpoint bootstraps MUST be written only at object boundaries, and
sequence numbers MUST strictly increase across all bootstrap copies on one
tape.

**Final-directory redundancy.** The final full-scope directory (the
`SidecarEpochDirectory` with `is_final_directory = true` covering the whole
tape, Section 10.5) MUST survive the loss of any single tape file. A `finish()`
(Section 11.3) MUST therefore write it as **at least two external full-scope
`parity_map` tape files** (Section 12.4), physically separated by at least
`S × m` blocks — the contiguous-damage tolerance of Section 1.1 — and never as
two adjacent tape files, so that no single tolerated burst can destroy both. The
final bootstrap MAY additionally carry the directory inline (key 20) or reference
one of these copies (key 21), but **a single inline or referenced copy MUST NOT
be the only non-bootstrap copy of the final directory**: were the final bootstrap
block lost, an inline directory is lost with it, and a lone referenced
`parity_map` is not reachable — structural discovery (Section 12.4) requires two
`parity_map` files to select among (a single discovered map yields no overlay).
Two full-scope `parity_map` copies are recovered by the Section 12.4 tie-break
whether or not the final bootstrap survives, and their content disagreement, if
any, is reported rather than silently resolved.

### 8.4. Discovery (Reader)

A Scanner with no off-tape state proceeds through the following discovery
strategies in order, stopping at the first bootstrap found. Strategies 1
and 4 are REQUIRED, as is strategy 2 whenever hints are available;
strategy 3 is OPTIONAL; a Scanner SHOULD offer strategy 5 as an explicit
opt-in.

1. **Beginning of tape (BOT)** (LBA 0) — always.
2. **Hint positions** supplied out of band (catalog, journal, medium
   auxiliary memory, operator).
3. **Heuristic fractional probing** (e.g. probing the 5%…95%
   marks of a total-size hint).
4. **A bounded forward scan** from each candidate position, of up to
   `MAX_BOOTSTRAP_SCAN_BLOCKS` (1024) blocks.
5. As a last resort, an explicit opt-in full
   filemark-walk scan of the tape.

When the block size is unknown, the discovery candidates (256 KiB, 512 KiB,
1 MiB) MUST each be applied as a real drive reconfiguration before reading;
a parsed bootstrap is accepted only if its `block_size_bytes` equals the
configured read size.

A conformant Writer for production media MUST use one of the discovery-candidate
block sizes. This closes the writer-legal set over the discovery set: every
conformant tape is discoverable from the media alone, with no out-of-band hint. A
Scanner MUST nevertheless accept an operator-supplied block-size hint and
apply it as a configured read size — the hint path serves damaged-media
recovery and nonconformant tapes, not Writer freedom.

**Test-geometry carve-out.** Conformance test vectors and other explicitly
labelled test geometries (Section 10.7) MAY use a smaller block size — the
published vectors use 4096-byte blocks — to keep the fixtures compact. Such a
tape records its `block_size_bytes` in the bootstrap like any other and is read
by supplying that size as a configured read size (the hint path above); it is not
required to be discoverable from media alone. A block size outside the
discovery-candidate set does not by itself make a tape nonconformant; only a
production Writer emitting one does.

Per-block scan rules, applied identically on the known-size and
candidate-size paths:

1. Magic miss: the Scanner MUST advance to the next block.
2. Parse failure: the Scanner MUST keep scanning.
3. Medium-error read (SCSI sense key 0x03, either sense format): the
   Scanner MUST skip the block and continue.
4. Filemark: the Scanner MUST continue into the next file.
5. End of data: there is no bootstrap at this position.
6. Any other transport error: the Scanner MUST abort discovery.
7. `drive_compression = true` on a parity bootstrap: the implementation
   MUST abort discovery and reject the tape (Sections 11.4, 16.3).

### 8.5. Authoritative Selection

When multiple bootstraps are found, the authoritative copy is the one with
the lexicographically greatest key

```text
(is_final_map, sequence, map_total_data_ordinals)
```

where a bootstrap with no digest record contributes `(false, sequence, 0)`;
ties keep the earlier find. The selected bootstrap's digest record then
governs map validation (Section 12.4), and its scheme record pins the
geometry every sidecar must agree with (Section 13.3).

A bootstrap copy whose frame parses but whose payload fails validation (header
or digest-record CRC, or the Section 10.5 directory invariants when it carries an
inline directory) MUST be treated as **not found** for this selection — never
selected as authoritative on the strength of a parsed frame alone. Selection then
falls through to the next readable copy and, if none governs the required scope,
to structural discovery (Section 12.4). This is what lets a lost or corrupt final
bootstrap fall back to the redundant full-scope `parity_map` copies (Section 8.3)
rather than pinning recovery to a damaged authority.

## 9. The Parity Sidecar Tape File

### 9.1. Structure

One sidecar is written per parity epoch, in its own tape file of
`total = 2H + P + 1` blocks, where `H` is the header/index copy block count
(Section 9.4) and `P = S × m` is the parity shard block count:

```text
blocks 0 .. H−1          primary header/index copy
blocks H .. H+P−1        parity shards, parity-index-major: shard i  ⇒
                         parity_index = i / S, stripe = i mod S
blocks H+P .. 2H+P−1     tail header/index copy
block  2H+P              footer locator
```

The parity shard for `(stripe, parity_index)` occupies the block at

```text
block(stripe, parity_index) = H + parity_index·S + stripe
```

using the **recorded scheme `S`** (Section 9.2 header field 0x24) — always the
constant scheme stripe count, never a per-epoch value, since every sidecar
carries exactly `P = S × m` parity blocks (Section 9.2) regardless of how many
data stripes the epoch actually fills. This **parity-index-major** placement
carries the Section 3.3 data interleave into the parity region: consecutive
parity blocks belong to consecutive stripes, so a contiguous burst crossing the
data→sidecar boundary spreads across stripes exactly as it does within the data
region (Appendix B.2). It is the sole reason the contiguous-damage guarantee of
Section 1.1 holds across that boundary and not merely within object data.

Parity shard blocks are raw blocks with no headers. Physical block placement is
determined solely by the locator above from a shard's explicit `(stripe_index,
parity_index)` fields; it is **independent of the order of that shard's entry in
the Section 9.3 index stream** (which remains stripe-major). A Reader MUST locate
a parity shard by computing `block(stripe, parity_index)` from its explicit
fields, never by the position of its index entry. The tail copy MUST
carry metadata and index content identical to the primary — only
`copy_kind` and the recomputed CRCs differ — and when both copies parse,
readers MUST verify they agree and reject divergence. The minimum block
size for sidecars is 0xC0.

### 9.2. The Header Block (block 0 of each copy) — all little-endian

| Offset | Len | Field | Constraint |
| --- | ---: | --- | --- |
| 0x00 | 8 | magic | HMAC(tape_uuid, `SIDECAR_MAGIC_LABEL`)[0..8] (Section 5.2) |
| 0x08 | 16 | tape_uuid | MUST match the bootstrap |
| 0x18 | 8 | epoch_id u64 | |
| 0x20 | 2 | k u16 | ≠ 0 |
| 0x22 | 2 | m u16 | ≠ 0 |
| 0x24 | 4 | S u32 | ≠ 0 |
| 0x28 | 4 | block_size u32 | MUST equal the actual block size |
| 0x2C | 4 | schema_version u32 | MUST be 1 |
| 0x30 | 8 | protected_ordinal_start u64 | |
| 0x38 | 8 | protected_ordinal_end_exclusive u64 | > start |
| 0x40 | 8 | logical_shard_count u64 | MUST = S × k |
| 0x48 | 8 | real_data_shard_count u64 | = end − start; ≤ logical_shard_count |
| 0x50 | 4 | parity_block_count u32 | MUST = S × m |
| 0x54 | 4 | data_crc_count u32 | MUST = real_data_shard_count |
| 0x58 | 4 | sidecar_header_block_count u32 (H) | MUST equal the recomputed layout (Section 9.4) |
| 0x5C | 4 | inline_index_entry_bytes u32 | MUST equal the recomputed layout (Section 9.4) |
| 0x60 | 8 | sidecar_total_block_count u64 | = 2H + P + 1 |
| 0x68 | 8 | primary_header_start_block u64 | MUST = 0 |
| 0x70 | 8 | tail_header_start_block u64 | MUST = H + P |
| 0x78 | 8 | footer_block_index u64 | MUST = 2H + P |
| 0x80 | 2 | copy_kind u16 | 1 = primary, 2 = tail |
| 0x82 | 2 | reserved | MUST be 0 |
| 0x84 | 4 | copy_generation u32 | MUST be 0 in version 1 |
| 0x88 | 32 | canonical_metadata_hash | Section 9.5 |
| 0xA8 | 8 | reserved u64 | MUST be 0 |
| 0xB0 | 8 | header_crc64 | CRC-64/XZ over bytes 0x00..0xB0 |
| 0xB8 | var | inline index entries | Section 9.3 |
| … | | zero fill | MUST be zero up to offset block_size − 8 |
| bs−8 | 8 | block0_crc64 | CRC-64/XZ over bytes 0..block_size−8 |

An epoch protects the half-open ordinal range
`[protected_ordinal_start, protected_ordinal_end_exclusive)`. The first
epoch starts at ordinal 0; subsequent epoch ranges MUST be contiguous, with
each start equal to the preceding end. Epoch ids MUST increase by one but
carry no range arithmetic. `real_data_shard_count` MUST equal
`protected_ordinal_end_exclusive − protected_ordinal_start`, MUST be in
`1..=S × k`, and a value below `S × k` marks a short epoch whose missing
logical positions are implicit zeros (Section 6.4). Short epochs are legal
at any checkpoint boundary, including mid-tape.

### 9.3. The Index Entry Stream

The index is packed binary (not CBOR): **all parity entries first**, in
stripe-major order (stripe 0 parity 0, stripe 0 parity 1, …, stripe 1
parity 0, …), **then all data-CRC entries** in ascending ordinal order:

- **Parity entry (16 bytes)**: u32 stripe_index; u16 parity_index; u16
  reserved, MUST be 0; u64 parity_shard_crc64.
- **Data-CRC entry (8 bytes)**: u64 data_shard_crc64 — one per *real* data
  shard only (implicit zeros carry no CRC).

There are exactly `S × m` parity entries and `real_data_shard_count`
data-CRC entries. The stream begins in block 0 immediately after the header
(offset 0xB8) and spills into blocks 1..H−1; **an entry never straddles a
block's usable area** (the area below the trailing CRC). Every index block —
block 0 and each spill block — ends with a u64 LE CRC-64/XZ over its bytes
0..block_size−8, and unused space below the CRC MUST be zero.

### 9.4. Index Layout Computation

`H` (`sidecar_header_block_count`) and `inline_index_entry_bytes` are fully
determined by `(block_size, S, m, real_data_shard_count)`. Readers MUST
recompute both and reject header values that disagree. The normative
algorithm — walk the entries in stream order, packing greedily, moving an
entry that would cross the usable limit entirely into the next block:

```text
limit  = block_size − 8                  (usable bytes per block, below the CRC)
offset = 0xB8                            (block 0: entries start after the header)
blocks = 1
inline = unset

for each entry e in stream order (parity entries, then data-CRC entries):
    len = 16 if e is a parity entry else 8
    if offset + len > limit:
        if blocks == 1 and inline is unset:  inline = offset − 0xB8
        blocks += 1
        offset  = 0                      (spill blocks: entries start at 0)
    offset += len

if inline is unset:  inline = offset − 0xB8
H = blocks
```

A block size too small to hold a single 16-byte entry in a spill block
(or the header plus trailing CRC) is invalid for sidecars (minimum 0xC0,
Section 2.5). A worked computation at the default geometry is in
Appendix A.2.

### 9.5. The Canonical Metadata Hash

`canonical_metadata_hash` is SHA-256 over, in order:

1. the domain string `"remanence-sidecar-metadata-v1"` (29 ASCII bytes, no
   terminator);
2. the header's exact wire bytes 0x00 through 0x7F inclusive — magic through
   `footer_block_index`, with `primary_header_start_block` as 0 — i.e.
   every field *before* `copy_kind`;
3. the exact wire bytes of every index entry in stream order (Section 9.3),
   without block padding or block CRCs.

Excluded by construction: `copy_kind`, the reserved fields, `copy_generation`,
the hash field itself, and all CRC fields. Both copies of one sidecar
therefore carry the same hash, and the epoch directory (Section 10.5) can
verify a surviving header copy independently of *which* copy survived.
Readers MUST verify the hash on every index parse.

### 9.6. The Footer Block (last block) — all little-endian

| Offset | Len | Field |
| --- | ---: | --- |
| 0x00 | 8 | magic = HMAC(tape_uuid, `SIDECAR_FOOTER_MAGIC_LABEL`)[0..8] |
| 0x08 | 2 | footer_version u16 = 1 |
| 0x0A | 2 + 4 | reserved, MUST be 0 |
| 0x10 | 16 | tape_uuid |
| 0x20 | 8 | epoch_id u64 |
| 0x28 | 8 + 8 | protected_ordinal_start / protected_ordinal_end_exclusive |
| 0x38 | 4 | H u32 (`sidecar_header_block_count`) |
| 0x3C | 4 | P u32 (`parity_shard_block_count`) |
| 0x40 | 8 | sidecar_total_block_count u64 |
| 0x48 | 8 | primary_header_start_block u64 = 0 |
| 0x50 | 8 | tail_header_start_block u64 = H + P |
| 0x58 | 32 | canonical_metadata_hash |
| 0x78 | 8 | footer_crc64 — CRC-64/XZ over bytes 0x00..0x78 |
| 0x80… | | zero fill, MUST be zero |

The footer is a *locator*: it holds everything needed to find and check
either header copy without reading the other, and it sits at the end of the
tape file where it is found by reading the file's last block (Section 12.3
item 4, Section 13.3).

## 10. The parity_map Tape File and the Sidecar Epoch Directory

### 10.1. Purpose

The **sidecar epoch directory** is the per-epoch repair root: for each
sidecar it records where it is, what range it protects, its layout counts,
and its `canonical_metadata_hash` — enough to locate and verify a surviving
header copy even when scan-time classification of that sidecar failed
(Section 13.3 step 3). The directory rides inline in the bootstrap (key 20)
when it fits; otherwise it is written as a separate **parity_map tape file**
referenced from the bootstrap (key 21).

### 10.2. Structure

With payload length `L` and `M = ceil((0xB8 + L) / block_size)` blocks per
copy:

```text
blocks 0 .. M−1       primary copy   (header at offset 0, payload from offset 0xB8)
blocks M .. 2M−1      tail copy      (identical content; copy_kind differs)
block  2M             footer locator
total = 2M + 1
```

The payload's bytes run contiguously from offset 0xB8 of the copy's first
block across the copy's subsequent blocks with no per-block framing; the
copy's unused tail MUST be zero. There are **no per-block CRCs** in a
parity_map: payload integrity is the header's `payload_sha256`, and
redundancy is the dual copy plus the footer locator (Appendix B.7). The
minimum block size for parity_map files is 0xB8.

### 10.3. Header and Footer Blocks — all little-endian

The header (first block of each copy) and the footer (last block of the
file) share one fixed 0xB8-byte region layout:

| Offset | Len | Field | Header | Footer |
| --- | ---: | --- | --- | --- |
| 0x00 | 8 | magic | HMAC(tape_uuid, `PARITY_MAP_MAGIC_LABEL`)[0..8] | HMAC(tape_uuid, `PARITY_MAP_FOOTER_MAGIC_LABEL`)[0..8] |
| 0x08 | 2 | version u16 | schema_version, MUST be 1 | footer_version, MUST be 1 |
| 0x0A | 2 | copy_kind u16 / reserved | 1 = primary, 2 = tail | reserved, MUST be 0 |
| 0x0C | 4 | reserved u32 | MUST be 0 | MUST be 0 |
| 0x10 | 16 | tape_uuid | MUST match | MUST match |
| 0x20 | 4 | sequence u32 | Section 10.4 | same |
| 0x24 | 4 | block_size u32 | MUST equal the actual block size | same |
| 0x28 | 8 | payload_len u64 | `L` | same |
| 0x30 | 32 | payload_sha256 | SHA-256 of the payload bytes | same |
| 0x50 | 32 | canonical_map_digest | the Section 7.3 digest | same |
| 0x70 | 4 | directory_scope_tape_file_count u32 | digest-record scope | same |
| 0x74 | 8 | directory_scope_total_data_ordinals u64 | digest-record scope | same |
| 0x7C | 8 | directory_scope_highest_protected_ordinal u64 | digest-record scope | same |
| 0x84 | 1 | is_final_directory u8 | MUST be 0 or 1 | same |
| 0x85 | 3 | pad | MUST be 0 | same |
| 0x88 | 8 | copy_block_count u64 | `M` | same |
| 0x90 | 8 | parity_map_total_block_count u64 | `2M + 1` | same |
| 0x98 | 8 | primary_copy_start_block u64 | MUST = 0 | same |
| 0xA0 | 8 | tail_copy_start_block u64 | MUST = M | same |
| 0xA8 | 8 | footer_block_index u64 | MUST = 2M | same |
| 0xB0 | 8 | crc64 | CRC-64/XZ over bytes 0x00..0xB0 | same |

Readers MUST validate the locator arithmetic (`M` from `payload_len` and
`block_size`; total = 2M + 1; the three start indices) and reject
disagreement between header, footer, and the measured tape file length.

### 10.4. Payload (CBOR)

A single integer-keyed map (Section 5.3):

```text
{1: tstr  "rem-parity-map-v1",
 2: bytes .size 16   tape_uuid,
 3: uint  sequence,
 4: map   SidecarEpochDirectory          (Section 10.5),
 5: bytes .size 32   canonical_map_digest,
 6: ?tstr writer_version,
 7: ?tstr write_timestamp}
```

Key 7, when present, uses the same [RFC3339] form as bootstrap key 4.

The decoded payload MUST match the header/footer locator fields (UUID,
sequence, digest, scope) and the payload bytes MUST hash to
`payload_sha256`. `sequence` is a per-tape counter over parity_map
emissions, strictly increasing per tape across sessions (a Resumer seeds the
next value above every parity_map sequence in the committed prefix,
Section 14), independent of the bootstrap sequence. In the presence of a
referencing bootstrap readers treat it as diagnostic — authority flows from
that bootstrap (Sections 10.6, 12.4); in the structurally discovered tier
(Section 12.4), where no bootstrap is usable, it is an ordering key of the
Section 12.4 selection.

### 10.5. The SidecarEpochDirectory (CBOR)

```text
{1: uint scope_tape_file_count,
 2: uint scope_total_data_ordinals,
 3: uint scope_highest_protected_ordinal,
 4: bool is_final_directory,
 5: [ entries ]}
```

Each entry:

```text
{1: uint tape_file_number,
 2: uint epoch_id,
 3: uint protected_ordinal_start,
 4: uint protected_ordinal_end_exclusive,
 5: uint sidecar_total_block_count,
 6: uint sidecar_header_block_count,        (H)
 7: uint parity_shard_block_count,          (P)
 8: bytes .size 32  canonical_metadata_hash,
 9: uint flags}
```

Flags: 0x01 = `FINAL_PARTIAL_EPOCH`; 0x02 = primary-copy-known-good; 0x04 =
tail-copy-known-good. `FINAL_PARTIAL_EPOCH` MUST appear only on the short
epoch emitted by terminal `finish()`; checkpoint-short epochs MUST leave it
clear. Readers MUST NOT treat an unflagged short epoch as invalid or as an
unfinished tape. Unknown flag bits MUST be rejected.

**Invariants (a decoder MUST validate all of them on every decode; any violation
is `DirectoryInvalid`, Section 15).** These restate at decode time what
Section 9.2 requires of the on-tape sidecar-header sequence, because a directory
MAY be trusted without reading those headers (Section 10.1):

- entries strictly ascending by `tape_file_number`, each `< scope_tape_file_count`;
- non-zero block counts;
- `scope_highest_protected_ordinal ≤ scope_total_data_ordinals` (the range
  `[scope_highest_protected_ordinal, scope_total_data_ordinals)` is the tail of
  committed-but-unprotected ordinals permitted by Section 11.2, and is empty on a
  `finish()`ed or checkpointed directory);
- the protected ranges **partition `[0, scope_highest_protected_ordinal)`**:
  taken in ascending `tape_file_number` order, the first
  `protected_ordinal_start` is `0`, each entry's `protected_ordinal_start` equals
  the previous entry's `protected_ordinal_end_exclusive` (contiguous, no gaps and
  no overlaps), every range is non-empty, and the last
  `protected_ordinal_end_exclusive` equals `scope_highest_protected_ordinal` (or
  the directory is empty and `scope_highest_protected_ordinal = 0`);
- `epoch_id` values are unique and consecutive starting from `0`
  (`0, 1, …, count−1`), matching the bare monotonic epoch counter of Section 2.3.

### 10.6. The ParityMapReference (bootstrap key 21)

```text
{1: uint tape_file_number,
 2: uint block_count,
 3: uint scope_tape_file_count,
 4: uint scope_total_data_ordinals,
 5: uint scope_highest_protected_ordinal,
 6: bool is_final_directory,
 7: bytes .size 32  payload_sha256,
 8: bytes .size 32  canonical_map_digest}
```

— enough to locate the parity_map tape file, bound its size before reading
it, and verify its payload without trusting it.

### 10.7. Inline versus External (Writer)

The directory rides inline in the bootstrap (key 20) if and only if the
fully framed bootstrap — base payload plus key 20 — fits the block with a
slack margin of 4096 bytes; the margin is waived below 8 KiB
blocks (an allowance for small test geometries). The fit check MUST be
performed by actually framing the candidate bootstrap and observing success
or the typed too-large failure (`BootstrapPayloadTooLarge`), never by
estimation.

When external, the ordering is fixed: the parity_map takes the next tape
file number `N`; the bootstrap takes `N + 1`; the directory and digest
scope cover `N + 2` files — both control files included in their own scope.
The payload is first encoded with a zeroed digest to fix `M`; the real
digest is then computed over the projected map and the payload re-encoded;
the resulting block count MUST be unchanged (the digest is fixed-size, so
re-encoding cannot change the payload length).

## 11. Writer Obligations

### 11.1. Commit Discipline (per tape file)

Every tape file — object, sidecar, bootstrap, parity_map — goes through one
cycle:

```text
begin                      (at the durable boundary; dense numbering; one in flight)
→ write blocks             (any short write / EOM / completion-unknown ⇒ abandon)
→ trailing filemark        (immediate or synchronous; same failure rule;
                           EOM here ⇒ abandon — never commit)
→ synchronization proof    (the filemark's synchronous completion, or a
                           later shared barrier — see below; under
                           deferral, this step and the two after it move
                           to the barrier for every file it covers)
→ filemark-map push        (the in-memory projected map gains the entry)
→ durable-boundary advance
→ [object close only] emit queued sidecars (each its own write cycle and
                           boundary advance)
→ off-tape commit record   (THE commit point, Section 3.4; at object close,
                           one record — or one durable transaction — covers
                           the object and every sidecar emitted at its close)
```

Consecutive cycles MAY defer synchronization to one shared **synchronizing
barrier** (Section 3.5). A barrier covers every tape file written since the
previous synchronization proof (or since session start); it MUST complete
before the commit record of any file it covers takes effect. The durable
boundary then advances for the whole batch at the barrier, and one commit
record — or one durable transaction — MAY cover the batch. A
completion-unknown, end-of-medium, or failed outcome at the barrier MUST
poison the writer, and every file the barrier would have covered remains
uncommitted. The per-file synchronous filemark of the basic cycle is the
one-file case of this rule.

A Writer MAY stage durable records for covered-but-unproven files before
the barrier (the reference journal does — Appendix B.12), but such staged
records are not commit records: replay MUST disregard any staged record
not covered by a subsequent durable commit marker written after its
barrier. A session that ends before a barrier commits nothing since the
last completed synchronization proof; resume proceeds per Section 14.

The object-close bundle is durable atomically: a crash before the bundle's
commit record leaves the object *and* the sidecars emitted at its close
beyond the durable boundary — a torn tail, physically superseded on resume
(Section 14) — so a committed prefix always satisfies the Section 11.2
bounded-restart rule.

Failure at any step MUST abandon the in-flight file (the boundary rolls
back) and **poison the writer**: a poisoned writer refuses every subsequent
operation on the session. One exception: a block-write failure whose
completion state is *known* (the failed block consumed no position) MAY
leave the writer usable; a completion-unknown failure MUST poison. The
watermark `W` advances only after a sidecar's boundary commit.

### 11.2. Epochs and Sidecars

Parity accumulates incrementally per data block (Section 6.3): the writer
holds `S × m` block-sized accumulators and one CRC per pending data block —
bounded memory regardless of object sizes. At `S × k` data blocks the epoch
closes into a **pending** sidecar held in memory or spool; no tape I/O
occurs mid-object. Pending sidecars are emitted as tape files when the
current object closes. A checkpoint barrier also closes a non-empty short
epoch, emits its sidecar without `FINAL_PARTIAL_EPOCH`, then writes the
non-final checkpoint edition. `finish()` performs the same funnel with a
terminal reason: its short epoch, if any, carries `FINAL_PARTIAL_EPOCH`, and
the following bootstrap is final. Implicit-zero positions never cause
padding blocks to be written (Section 6.4).

After each object's close-and-emit bundle, unprotected ordinals MUST number
fewer than `S × k` — the version-1 bounded-restart rule: at most one open
epoch ever needs rebuilding (Section 14 step 2). A checkpoint leaves no open
epoch, so the next epoch starts at the checkpoint's protected end with the
next monotonic epoch id; checkpoint cadence is independent of `S × k`.

### 11.3. checkpoint() and finish()

`checkpoint()` — permitted only between objects; a mid-object call MUST be
refused — writes a non-final bootstrap carrying a prefix-scoped digest and
commits a control record: the clean resumable boundary.

`finish()` closes the partial epoch, emits all remaining sidecars, writes
the final bootstrap (`is_final_map = true`, full-scope digest, inline
directory or parity_map per Section 10.7), and commits. A finished tape
accepts no further appends.

### 11.4. Session Preconditions

Drive hardware compression MUST be verified off — set, then read back —
before any parity write; the effective value is recorded as bootstrap key 5,
and a parity bootstrap recording `true` MUST be rejected by readers and
writers alike (Section 16.3). Capacity admission (early-warning reserve
policy) is out of scope, but a writer MUST treat hard end-of-medium
mid-file as a commit failure (Section 11.1), never as a signal to truncate
an object.

## 12. Scanner Obligations

### 12.1. Inputs and Authority

If an off-tape catalog supplies a map and it validates — the catalog's tape
UUID matches the identity read from a bootstrap on the mounted tape,
`W ≤ T`, and the map's sidecar-derived watermark equals its recorded `W` —
that map is authoritative and no physical walk occurs within its scope
(confirming the tape's identity still requires reading one bootstrap;
tail classification beyond the map scope requires walking, Section 12.6). Otherwise the
Scanner walks the tape from LBA 0.

### 12.2. The Walk

Per tape file: read the head block; measure the file's length by filemark
spacing (space to the next filemark; the file's block count is the position
delta minus one); a zero-block file or a missing trailing filemark is
structural damage; EOD at a file start ends the walk.

### 12.3. The Classification Ladder (in order)

1. **Bootstrap**: the fixed magic matches, the full frame parses, the
   frame's `block_size_bytes` equals the read size, and the file measures
   exactly 1 block.
2. **parity_map**: the primary header parses (per-tape derived magic); the
   measured block count MUST equal the header's
   `parity_map_total_block_count` (mismatch is a hard error). A Scanner
   SHOULD also probe the tail copy and footer when the primary is
   unreadable, locating them from the measured file length.
3. **Sidecar (primary)**: the primary header parses; the measured block
   count MUST equal the header's `sidecar_total_block_count` (mismatch is a
   hard error, as for a parity_map).
4. **Sidecar (footer/tail probe)**: read the file's last block; if it
   parses as a sidecar footer, the footer's total MUST equal the measured
   block count; then verify the tail header copy against the footer,
   field for field. Classification MAY fall back to footer fields alone if
   the tail copy is unreadable.
5. **Object, by elimination** — never by reading object content.

In items 2 through 4, a count-mismatch “hard error” is scoped to that rung and
that tape file's classification: the Scanner reports the failed
classification and continues the walk with the next tape file. It MUST NOT
abort the whole walk for that mismatch.

**Unreadable head block:** the Scanner MUST NOT abort. It MUST measure the
file by filemark spacing, run the footer/tail sidecar probe, and otherwise
classify the file as an object candidate. This is the no-circular-failure
rule in action: the block needing recovery may be the very block that would
have classified the file.

### 12.4. Overlay and Validation

After the walk, the Scanner applies the authoritative directory overlay, in
priority order: the bootstrap's inline directory (key 20) → the bootstrap's
`ParityMapReference` (key 21: read the referenced file, verify the payload
SHA-256 and every cross-check; on any failure fall through) → a
structurally discovered parity_map → none.

**Selecting among structurally discovered parity_maps.** When the overlay
falls through to structurally discovered parity_map files — no bootstrap
inline directory and no readable `ParityMapReference` usable over the
recoverable prefix — and more than one parity_map tape file survives, the
Scanner selects deterministically.

1. **Integrity gate.** A candidate parity_map is eligible only if: its
   `tape_uuid` matches the tape identity (Section 12.1); at least one header
   copy parses with a valid magic and header CRC and satisfies the
   Section 10.3 locator arithmetic against the measured tape-file length
   (`M` from `payload_len`/`block_size`; total = `2M+1`; the three start
   indices); its payload bytes — taken from the copy whose header parsed,
   falling back to the other copy on a payload-block read failure
   (Appendix B.7) — hash to the header's `payload_sha256`; and the decoded
   payload matches the header/footer locator fields (`tape_uuid`,
   `sequence`, `canonical_map_digest`, the three scope scalars) with the
   Section 10.5 directory invariants satisfied (Sections 10.4, 10.5, 12.3
   item 2). Header–footer agreement is required only when the footer block
   is readable; one valid header copy with an unreadable footer is eligible
   (the footer is redundancy — Appendix B.7, Section 1.1 goal 5). A
   candidate failing any condition MUST be discarded.

2. **Walk cross-check.** For each eligible candidate, apply its own
   `SidecarEpochDirectory` to the walked filemark map as the overlay above
   does — re-type the directory-listed tape files as sidecars with their
   epoch and range (block counts MUST match; a directory entry conflicting
   with a *scanned bootstrap or parity_map* classification is a hard error),
   re-type every in-scope head-unreadable 1-block tape file the directory
   does not list as a sidecar as a bootstrap (deterministic given the
   directory, not the single-candidate search of the re-typing paragraph
   below), renumber object ordinals, and truncate to
   `directory_scope_tape_file_count`. A candidate MUST be discarded if the
   walk yields fewer than `directory_scope_tape_file_count`
   structurally-complete tape files, or if it declares
   `is_final_directory = 1` and its scope does not equal the count of
   structurally-complete tape files. Then recompute the Section 7.3
   canonical digest and the Section 7.2-derived `T`/`W` over the overlaid,
   scoped prefix and compare all four values (digest, `tape_file_count`,
   `total_data_ordinals`, `highest_protected_ordinal`) to the candidate's;
   discard a candidate whose overlaid projection does not reproduce them. A
   tape carrying within one scope both a head-damaged 1-block object and a
   head-damaged bootstrap defeats the deterministic re-typing and its
   parity_map is discarded — a multi-fault case beyond the Section 1.1
   goal-5 single-fault contract, failing safe.

3. **Selection.** Among candidates passing steps 1 and 2, the authoritative
   parity_map is the one with the lexicographically greatest key
   `(is_final_directory, sequence, directory_scope_total_data_ordinals)` —
   the parity_map analogue of the Section 8.5 bootstrap key (Section 10.4);
   the step-2 `is_final_directory` guard makes this key select the largest
   validated scope. If two candidates share the key, the one with the lowest
   `tape_file_number` (Section 3.1) is authoritative; two candidates sharing
   the key but disagreeing on content is a structural inconsistency the
   Scanner MUST report — the candidate positions, the shared key, and the
   chosen tape file — while proceeding with the lowest-`tape_file_number`
   copy.

The Section 12.4-selected parity_map's own digest record is the fencing
authority for its validated scope (Sections 12.6, 13.2); a bootstrap digest
record covering a subset of that scope is cross-checked for content
agreement over its overlapping prefix only, and a disagreement is a hard
conflict the Scanner reports — never a reason to shrink the parity_map's
scope. If no candidate passes steps 1 and 2, the overlay is `none` and each
sidecar retains its scanned classification and range (the fallback of the
overlay-precedence paragraph above).

The overlay re-types directory-listed tape files as sidecars with their
epoch and range (block counts MUST match; a directory entry conflicting
with a *scanned* bootstrap or parity_map classification is a hard error).
A tape file the walk classified as a sidecar but absent from the directory
within its scope retains its scanned classification and range; the scope
scalars and the canonical digest remain the arbiters. The overlay then
renumbers object ordinals accordingly, truncates the map to the directory
scope, and cross-checks the scope totals.

Finally the Scanner validates the map against the authoritative bootstrap's
digest record (Sections 7.4, 8.5): recompute the canonical digest over the
scoped prefix and compare the digest and all three scalars. A mismatch is
fatal to that map — not to the tape: a different bootstrap copy may carry a
usable scope.

Before declaring `FilemarkMapDigestMismatch`, a Scanner SHOULD attempt
**bootstrap re-typing**: a destroyed bootstrap block is structurally
indistinguishable from a 1-block object with an unreadable head, so for
each 1-block tape file classified as an object by elimination because its
head block was unreadable, re-hypothesize its kind as Bootstrap (block
count 1, no ordinal), renumber the object ordinals, and revalidate;
accept the first hypothesis whose digest and scope scalars validate. The
hypothesis space is bounded by the number of unreadable 1-block files. The
reference Scanner tests candidates one at a time in ascending tape-file order;
it does not re-type readable corrupt headers, genuine readable 1-block
objects, or combinations of candidates. Each hypothesis is passed through the
same directory overlay, ordinal renumbering, digest, and scope-scalar checks as
the original scan.
Without re-typing, single-block damage to a checkpoint bootstrap would
invalidate every digest scope covering it, defeating the isolation goal
of Section 12.5.

### 12.5. Epoch Isolation

Damage confined to one sidecar's metadata — any or all of its header
copies, its footer, its directory entry's health flags — MUST NOT degrade
classification, mapping, digest validation, or recovery of any other epoch.
At worst the damaged epoch becomes "metadata unavailable"
(`SidecarMetadataUnavailable`, scoped to that epoch by definition). Copy
health is deliberately excluded from the canonical digest so that
*discovering* damage never invalidates the map (Section 7.3).

### 12.6. The Tail Beyond the Attested Prefix

This section defines what a walking Scanner (Section 12.2) may conclude
from the cartridge alone; its conclusions rest on digest records. A tape
carrying no digest record anywhere (the no-parity minimum of Section 8.2)
offers no attestation, and a bare-tape map of it is entirely forensic. A
Scanner seeded by a validated catalog map (Section 12.1) is outside this
section: that map's authority is off-tape (Section 16.1) and may
legitimately extend past the newest on-tape attestation; such a Scanner
needing tail classification extends the walk beyond the map scope.

The **attested prefix** of a tape is the largest validating scope among
the tape's validating digest records: the bootstrap digest records, ranked
among themselves in descending Section 8.5 order, and — when no bootstrap
supplies a directory usable over the recoverable prefix — the
Section 12.4-selected structurally discovered parity_map digest record; with
validation and bootstrap re-typing per Section 12.4. A bootstrap digest
record is higher authority only at equal scope, winning a content tie over
the same prefix; a smaller-scope bootstrap digest record does not reduce a
larger validating scope. The attested prefix equals that validated scope of
Section 7.4; a validating final bootstrap is normally itself the
largest-scope record. An attesting bootstrap
whose own tape file is structurally incomplete (its trailing filemark
missing) cannot complete its own map entry, so its digest does not
validate; selection falls to the next candidate. If no digest record
validates — including, after the Section 12.4 fallback, no structurally
discovered parity_map — the attested prefix is empty. From the cartridge alone, every
tape file then falls into exactly one class:

- **Attested** — within the attested prefix: eligible as recovery input
  under Sections 12 and 13 (attested includes files awaiting Section 13
  parity repair; Section 13.2's watermark fence still applies to ordinals
  at or beyond `W`). Attestation proves presence and self-consistent
  structure, not authenticity (Section 16.1) and not commitment: a crash
  between a barrier and its commit record can leave an attested batch the
  Writer never acknowledged (Section 3.4).
- **Unattested** — beyond the attested prefix but structurally complete
  (measurable head-to-filemark; the ladder of Section 12.3 still
  classifies it). A Scanner MUST exclude unattested files from the
  validated map it reports, and a Recoverer MUST NOT use them as
  reconstruction inputs. A Scanner SHOULD report unattested files (count
  and positions). An implementation MAY offer explicit opt-in
  payload-level salvage of unattested object files — possible only before
  a resume, since a Resumer physically supersedes the tail (Section 14).
  Salvage output MUST be identified as salvage, distinct from Recoverer
  results (Sections 13.5, 15), and is not a recovery input in the sense of
  Sections 3.4 and 14; whether a salvaged payload is usable is the payload
  format's determination (Section 4.3), not this format's. Unattested
  non-object files are excluded from salvage: nothing past the attestation
  is trusted to describe other files.
- **Truncated** — beyond the attested prefix and structurally incomplete
  (a missing trailing filemark, a zero-block file, or EOD inside the
  file): the physical artifact of an interrupted session; forensic only.
  (The *torn tail* of Sections 11.1 and 14 is the whole uncommitted tail —
  it includes Unattested files as well as Truncated ones.)

When the attested prefix is the whole tape (a validating final bootstrap,
Section 8.3), the Unattested and Truncated classes are empty. Every tail
state is thus decidable from the cartridge alone. The residual ambiguity —
whether a file past the newest attestation was ever committed — is exactly
the bare-tape cost stated in Appendix B.8; a Writer bounds it with its
checkpoint cadence (Sections 8.3, 11.3), and a Writer that never
checkpoints leaves the whole tape unattested until `finish()`.

## 13. Recoverer Obligations

### 13.1. Inputs

A validated, scoped map (Section 12); the bootstrap's scheme record; and
the failed addresses — `(tape_file_number, object_block_index)` pairs or
ordinals.

### 13.2. Fail Before I/O

Before any tape read, the Recoverer MUST reject, as typed refusals distinct
from recovery failures: ordinals outside the validated scope
(`OutsideValidatedMapPrefix`); ordinals ≥ `W` — the pending epoch, whose
parity does not exist yet (`UnrecoverablePendingEpoch`); and failed blocks
or sidecars in tape files outside the durable boundary.

### 13.3. Acquiring the Sidecar Index

Locate the epoch's sidecar tape file via the map, then, in order:

1. **Footer first.** Read the file's last block. If it parses as the
   epoch's footer and its total matches the map entry: read and verify
   **both** header copies against the footer locator, including the
   `canonical_metadata_hash`; use the primary if valid, else the tail;
   record copy health (both-usable / tail-lost / primary-lost).
2. **Primary fallback.** If the footer is unreadable, unparseable, **or
   inconsistent with the map entry** — a footer that parses but contradicts
   the map is treated as an invalid footer, not as a hard stop — fall back to
   the primary header at block 0 (copy-kind and map-entry block-count
   cross-checks apply).
3. **Directory-assisted tail rescue.** If the primary also fails and an
   epoch-directory entry is available: locate the tail copy at block
   `sidecar_total_block_count − 1 − sidecar_header_block_count` using the
   entry's counts, and verify its `canonical_metadata_hash` against the
   entry. The directory carries exactly the counts and hash needed to find
   and verify the tail copy without the footer — the case it exists for
   (Section 10.1).
4. Only when no header/index copy can be validated is the epoch
   **metadata-unavailable** — and only that epoch (Section 12.5).

This is the **recovery-usable rule**: at least one valid header/index copy
plus CRC-passing needed shards ⇒ the epoch is usable. The acquired index
MUST then be pinned against the bootstrap's scheme record (`k`, `m`, `S`,
block size) and the map entry's ordinal range; disagreement is
`SchemeMismatch`.

### 13.4. Erasure Taxonomy

For each stripe containing a failed block, gather the stripe's peers. Each
peer position is exactly one of:

- **Trusted shard**: the read succeeded, AND its CRC-64 matches the sidecar
  index (data CRC for data peers, parity CRC for parity peers), AND — for
  data peers — its object tape file is inside the durable boundary (in
  catalog-less recovery, inside the validated map scope; Section 13.2).
- **Erasure**: a read failure, a CRC mismatch, or a position outside the
  durable boundary. An erasure is *never* a trusted shard and never poisons
  the session.
- **Implicit zero**: an ordinal ≥ `protected_ordinal_end_exclusive` — an
  all-zero shard supplied without tape I/O; not an erasure (Section 6.4).

### 13.5. Reconstruction and Release

Reconstruct per Section 6.5 from the first `k` trusted or implicit shards
(data shards first in index order, then parity). More than `m` erasures in
a stripe is unrecoverable — a typed result carrying the stripe and the
counts (`Unrecoverable{stripe, lost_count, limit}`).

Every reconstructed data block MUST be verified against its sidecar data
CRC before release. A mismatch is an unrecoverable result — typed
distinguishably from parse failures and refusals, e.g. as `Unrecoverable`
with the stripe and counts — even though the
matrix algebra succeeded: it means some trusted input was wrong, and
releasing the output would convert detected damage into silent corruption.

### 13.6. Bulk Recovery (Informative)

A bulk Recoverer working a damaged region should plan per epoch, read each
needed peer at most once per planning window, and read in physical tape
order. As an illustration, one implementation bounds its planning windows at
1024 stripes and its recovery cache at 8 GiB; both are
quality-of-implementation choices, not format rules.

## 14. Resumer Obligations

A later session appends **after the last committed tape file** — not after
the last object, and not at the watermark.

1. Derive the committed prefix from the off-tape commit records, dropping
   any torn tail, and compute `W` and `T` from it.
2. Enforce the version-1 bound: `T − W < S × k` (at most one open epoch).
   `W ≤ T`; committed sidecar ranges MUST be contiguous from zero through
   `W`; epoch ids MUST be consecutive; and the prefix's final object entry
   MUST end exactly at `T`. A violation is `ResumeAppend`.
3. Rebuild the open epoch by **re-reading ordinals `[W, T)` from the
   committed prefix on tape** — a boundary or short read where data is
   expected is fatal — recomputing per-block CRCs and re-accumulating
   parity. (Under the step-2 bound, `[W, T)` is the next open epoch range;
   a committed prefix never contains a complete unprotected epoch,
   because an object and the sidecars emitted at its close commit as one
   bundle — Section 11.1.)
4. **Position to the append point (`Σ(block_count + 1)` over the prefix) and
   verify it by a positional query before writing anything.** The step-3 re-read
   of `[W, T)` crosses filemarks and tape-file boundaries, after which position
   MUST be re-synchronised by a positional query (Section 3.5); a write issued at
   a dead-reckoned, unverified position could land over committed data or short
   of the append point. No block is written until this verification succeeds.
5. Seed the writer with: the prefix map; the durable boundary; `W`; the next
   bootstrap sequence, strictly greater than every bootstrap sequence in the
   committed prefix (and at least the count of committed bootstraps); the next
   parity_map sequence, strictly greater than every parity_map sequence in the
   committed prefix; the live open-epoch state (`[W, T)`, shape- and
   CRC-revalidated, then re-accumulated) carried as live writer state; and
   **directory entries covering every committed-prefix sidecar one-for-one**, so
   every later bootstrap or parity_map directory still enumerates pre-crash
   epochs — the root-of-trust completeness rule. The incomplete open epoch
   `[W, T)` MUST NOT be closed or emit a sidecar at resume time: under the step-2
   bound a committed prefix never contains a complete unprotected epoch, so
   `[W, T)` is always a partial epoch, re-accumulated into live state, that emits
   its sidecar only when it later closes through the normal Section 11.1 cycle —
   whose **decode-what-you-wrote** round-trip (the encoded sidecar MUST re-parse
   to the planned header, index, and shard bytes before its blocks, filemark, and
   post-barrier position check commit as one bundle) applies at that close.

Anything physically on tape beyond the committed prefix is superseded by
the next append and MUST NOT be trusted for recovery.

## 15. Errors

Implementations SHOULD expose typed errors equivalent to the taxonomy
below. Names are normative for the test-vector manifests (Section 17);
surface syntax is not.

```text
NoBootstrapFound                discovery exhausted every strategy (Section 8.4)
NoBootstrapAtPosition           bounded scan at one position found nothing
BootstrapParse                  bootstrap frame or payload violates Section 8
BootstrapPayloadTooLarge        framed payload cannot fit the block (Section 10.7)
SidecarParse                    sidecar structure violates Section 9
SidecarMetadataUnavailable{epoch_id}   no header/index copy validated (Section 13.3)
ParityMapParse                  parity_map structure violates Section 10
DirectoryInvalid                SidecarEpochDirectory violates a Section 10.5 invariant
SchemeMismatch                  sidecar geometry disagrees with the bootstrap scheme
FilemarkMapDigestMismatch       recomputed digest or scope scalars disagree (Section 12.4)
FilemarkMapReconstruct          the walk or overlay could not produce a valid map
OutsideValidatedMapPrefix       refusal: address beyond the validated scope (Section 13.2)
UnrecoverablePendingEpoch       refusal: ordinal ≥ W, parity not yet written
Unrecoverable{stripe, lost_count, limit}   more than m erasures in a stripe
ReedSolomon                     matrix inversion or codec failure
CapacityReserveExceeded         writer admission policy refusal (policy-defined)
ObjectTooLargeForEmptyTape      writer admission refusal (policy-defined)
ResumeAppend                    Section 14 invariant violation
DriveCompressionEnabled         compression detected on / recorded for a parity tape
DriveCompressionModeUnknown     compression state could not be verified
Invariant                       internal consistency failure (implementation defect)
TapeIo                          transport/medium failure (not a format violation)
Journal                         commit-store failure (not a format violation)
```

Refusals (Section 13.2), parse failures, and reconstruction failures MUST
remain distinguishable; I/O faults MUST remain distinct from format
violations. Code paths reachable from tape bytes MUST NOT panic, crash, or
allocate unboundedly: every length that drives an allocation MUST be
cross-checked against a physically measured block count first
(Section 16.2).

## 16. Security Considerations

### 16.1. No Authentication

HMAC-derived magics bind blocks to a tape UUID and a role; they are **not**
authentication — the UUID is public (it is in the bootstrap), so anyone
with the tape can forge consistent structures. CRCs and SHA-256 digests
detect corruption, not tampering. The trust anchors are external: an
off-tape catalog or audit chain, and content-level verification in the
payload format. A tape that self-validates proves self-consistency only.

### 16.2. Hostile-Input Posture

All tape bytes are untrusted. Normative bounds: every declared count or
length is validated against the measured physical extent before any
allocation or seek it would drive; all arithmetic on tape-derived values is
checked; reserved fields and declared zero-fill MUST be verified zero
(misuse of reserved space is nonconformance, and silent acceptance would
foreclose 1.x extensions; the sole exception is the bootstrap's trailing
fill, which is excluded from acceptance decisions — Section 8.1); CBOR decoding enforces the Section 5.3 subset.
Implementations SHOULD fuzz the bootstrap, sidecar, and parity_map parsers
and the scan walk (Section 18).

### 16.3. Compression Interaction

Parity correctness assumes block-to-media identity: the damage geometry
model (Section 3.3) is meaningful only if the Nth logical block occupies
the Nth physical block's worth of media. Hardware compression silently
breaks that correspondence while appearing to work. Hence the dual defense:
the writer verifies compression off before writing (Section 11.4), and the
recorded `drive_compression` flag (bootstrap key 5) makes a tape written
with compression enabled identify itself as nonconformant — readers MUST
reject it.

### 16.4. Structure Leakage and Confidential Payloads

This format's structures are plaintext on the tape and content-blind at the
block layer, but they still reveal *shape*: the number of objects, each
object's block count, the write timeline (bootstrap timestamps and
sequences), and per-block CRC-64 values of stored bytes. An unkeyed CRC of
a stored block can confirm a guessed block's content; deployments for which
payload confidentiality matters SHOULD store objects in an encrypted
representation (for example, [RAO] encrypted objects), making every stored
block — and therefore every CRC and parity computation — a function of
ciphertext.

Bootstrap key 30 (Section 8.2.1) is the designated bounded surface for
payload-binding object rows. A plaintext RAO row exposes manifest location,
manifest size, manifest chunk count, and manifest digest; this is acceptable
because plaintext RAO objects are not confidential against a tape reader.
An encrypted RAO row exposes only recipient epoch ids, `metadata_frame_len`,
and `key_frame_len`, all already plaintext in the RAO encrypted envelope
stored in the same tape file. The encrypted row MUST NOT carry plaintext
manifest anchors
(`manifest_first_chunk_lba`, `manifest_size_bytes`, `manifest_chunk_count`,
or `manifest_sha256`), because those values describe confidential inner
content and would add leakage beyond the RAO envelope.

## 17. Test Vectors

Static test vectors are distributed alongside this specification, each with
a manifest recording inputs, the expected values, and — for negative
vectors — the expected Section 15 error name. Vectors use small geometries
(e.g. k = 2, m = 2, S = 2, 4 KiB blocks) so complete tape images are
practical to pin; at least one header-level vector MUST use the default
geometry parameters. Negative vectors contain exactly one fault each.

The companion archive is `remanence-test-vectors.tar`, SHA-256
`fa8570d31d3869155c9a2b4322b0846a5f5b2eb845d08c89ab4a78bcbb5e668f`.
Its `MANIFEST.tsv` inventories every contained vector manifest and generated
artifact, `CHECKSUMS.sha256` authenticates them, and the included `verify.py`
checks the archive without a source checkout. The archive is reproducibly
generated with the `publication-test-vectors` build target. The REM-PARITY
`vectors.json` records deterministic inputs, expected outputs or Section 15
typed errors, artifact hashes, and a checksum for every vector.

The arithmetic vectors stated in this document — the Section 5.1 CRC
values, the Section 6.8 Reed–Solomon values, and the Section 7.3 canonical
digest — are normative now and independently re-derivable from this
document alone. Image-level pinned bytes are **[pinned-at-generation]**:
produced by an implementation when the vectors are first
generated, independently re-derived, then frozen (Section 18 criterion 2).

**Positive vectors.** The Section 6.8 codec values plus full-stripe
reconstruction for every erasure pattern up to `m`; the Section 5.1 CRC
values; the Section 5.2 derived magics for a pinned sample `tape_uuid`; the
Section 7.3 digest vector plus one multi-epoch map; a complete
minimal tape image (bootstrap + one object + one sidecar + final bootstrap)
byte-pinned with its digest chain; a final-partial-epoch image exercising
implicit zeros; an external parity_map image (inline overflow); a no-parity
bootstrap; a checkpoint (prefix-digest) image; and a resume round-trip
image (committed prefix → reopened → appended). The suite also includes a
short epoch with `R = 1 < S = 2`, and a no-parity image whose schema-minor 3
bootstrap object row carries RAO-TV-P1's 36-byte UUID-string `object_id`
verbatim.

**Negative vectors (each single-fault).**

- *Bootstrap*: bad magic; schema_major = 2; header-CRC bit flip;
  payload-CRC bit flip; payload truncation; keys 20 and 21 together;
  `drive_compression = true` with parity; oversize payload; a 65-byte
  object-row `object_id` (`BootstrapParse`, object-id length).
- *Sidecar*: each header constraint of Section 9.2 violated (one vector per
  MUST); an index entry straddling a block's usable area; a spill-block CRC
  flip; nonzero reserved or fill bytes; primary/tail copy disagreement;
  footer total disagreeing with the map entry.
- *parity_map*: payload SHA-256 mismatch; locator/header disagreement;
  directory invariant violations (unknown flag bit, non-ascending entries,
  watermark mismatch).
- *SidecarEpochDirectory*: overlapping ranges; gapped ranges; duplicate epoch
  id; nonzero first protected-range start. Each is a validly framed bootstrap
  image whose sole semantic fault MUST produce `DirectoryInvalid`.
- *Digests*: one structural-field flip per digest scalar
  (`tape_file_count`, `map_total_data_ordinals`,
  `highest_protected_ordinal`).
- *Recovery*: m + 1 erasures (typed unrecoverable with counts); a corrupt
  peer counted as an erasure and then recovered around; a
  reconstructed-block CRC mismatch; a pending-epoch refusal; an
  outside-prefix refusal.
- *Damage matrix*: for the minimal image (and the external parity_map image
  for the parity_map-primary cell), single-block damage at each of —
  the object's head block; the sidecar primary header; the sidecar footer;
  the sidecar footer **and** primary (directory-assisted tail rescue); the
  parity_map primary; one bootstrap copy (exercising the Section 12.4
  bootstrap re-typing rule when the damaged copy lies inside a later digest
  scope) — each asserting the specified
  outcome (recovered / copy-health downgrade / one-epoch unavailability),
  never whole-tape failure.

The following boundary-burst rows are normative. A span counts the data
block, intervening filemark record, sidecar metadata blocks, and parity
blocks. The pinned small-geometry images use `m = 2`, `S = 2`, and `H = 1`;
the short row uses `R = 1`.

| Vector | Burst span | Required outcome |
| --- | --- | --- |
| `boundary-straddling-burst-m-limit` | `m·S + H + 1 = 6` records, beginning at the full epoch's last data block | recovered; the straddled stripe loses exactly `m = 2` shards |
| `boundary-straddling-burst-m-plus-one` | `m·S + H + 2 = 7` records at the same boundary | `Unrecoverable { stripe: 1, lost: 3, limit: 2 }` |
| `short-epoch-boundary-burst-unrecoverable` | `(m−1)·S + R + H + 2 = 6` records, with `R < S` | `Unrecoverable { stripe: 0, lost: 3, limit: 2 }` |

## 18. Conformance and Freeze Criteria

This specification is a draft until all of the following hold; after
freeze, no normative change is permitted other than errata that do not
change the set of valid tapes (anything else is version 2):

1. At least one complete implementation implements this document in every
   role — Writer, Scanner, Recoverer, Resumer, Verifier — with no known
   divergences from this document.
2. The Section 17 fixtures are present in the companion archive and pass, including the
   single-block damage matrix and the byte-pinned minimal tape image. Every
   **[pinned-at-generation]** value is independently re-derived by a second
   implementation (different language or library) before freezing, so a
   reference-implementation bug cannot be frozen into the conformance
   anchor.
3. Coverage-guided fuzzing of the bootstrap, sidecar, and parity_map
   parsers and of the scan walk reaches a corpus plateau with no panics,
   hangs, or unbounded allocations.
4. A live round-trip passes on real or virtualized tape hardware: write
   with injected damage (a fault-injecting transport), scan catalog-less, recover,
   and verify — at two distinct block sizes.
5. A long-term-recovery drill: an independent party reconstructs the
   minimal tape image's map and recovers one damaged block using only this
   document and a generic CBOR/SHA-256/HMAC toolkit — including re-deriving
   the Cauchy matrix from Section 6.
6. Accelerated arithmetic (table- or SIMD-based GF(2⁸) and CRC kernels) is
   proven byte-identical to the Section 5.1/6.1 definitions via the
   Section 17 vectors. Not a format change — but freeze SHOULD wait for it,
   so adopting an accelerator never silently changes emitted bytes.

## 19. IANA Considerations

This document has no IANA actions. The identifiers this specification
defines — the bootstrap magic, the `rs-cauchy-gf256-v1` erasure-scheme
identifier, the `rem-parity-map-v1` format identifier, the HMAC magic
labels, and the tape-file kind codes — are assigned by this document and
governed by its versioning rules; no registry is established or required.

## 20. References

### 20.1. Normative References

- [RFC2119] — Bradner, S., "Key words for use in RFCs to Indicate
  Requirement Levels", BCP 14, RFC 2119, March 1997,
  <https://www.rfc-editor.org/info/rfc2119>.
- [RFC8174] — Leiba, B., "Ambiguity of Uppercase vs Lowercase in RFC 2119
  Key Words", BCP 14, RFC 8174, May 2017,
  <https://www.rfc-editor.org/info/rfc8174>.
- [RFC2104] — Krawczyk, H., Bellare, M., and R. Canetti, "HMAC:
  Keyed-Hashing for Message Authentication", RFC 2104, February 1997,
  <https://www.rfc-editor.org/info/rfc2104>.
- [RFC3339] — Klyne, G. and C. Newman, "Date and Time on the Internet:
  Timestamps", RFC 3339, July 2002,
  <https://www.rfc-editor.org/info/rfc3339>.
- [RFC3629] — Yergeau, F., "UTF-8, a transformation format of ISO 10646",
  STD 63, RFC 3629, November 2003,
  <https://www.rfc-editor.org/info/rfc3629>.
- [RFC8949] — Bormann, C. and P. Hoffman, "Concise Binary Object
  Representation (CBOR)", STD 94, RFC 8949, December 2020,
  <https://www.rfc-editor.org/info/rfc8949>.
- [FIPS180-4] — National Institute of Standards and Technology, "Secure
  Hash Standard (SHS)", FIPS PUB 180-4, August 2015 (defines SHA-256),
  <https://doi.org/10.6028/NIST.FIPS.180-4>.

CRC-64/XZ is fully parameterized in Section 5.1; no external reference is
required to implement it.

### 20.2. Informative References

- [RAO] — "Rem Archive Object (RAO) Format, Version 1.0", companion
  specification published alongside this document: the reference payload
  format of Section 4.4.
- [LTO-SCSI] — International Business Machines Corporation, "IBM LTO
  Ultrium Tape Drive SCSI Reference", document GA32-0928: fixed-block I/O,
  sense data formats, and boundary classification for the Section 3.5 I/O
  layer.
- [RFC9562] — Davis, K., Peabody, B., and P. Leach, "Universally Unique
  IDentifiers (UUIDs)", STD 97, RFC 9562, May 2024,
  <https://www.rfc-editor.org/info/rfc9562>.

---

## Appendix A. Worked Examples (Informative)

### A.1. The Default Geometry

At the default scheme (k = 128, m = 4, S = 512) with 256 KiB blocks:

- One epoch protects `E = S × k = 65,536` data ordinals = 16 GiB of object
  data.
- Its sidecar carries `P = S × m = 2,048` parity shards = 512 MiB, a 3.125%
  overhead.
- Contiguous damage tolerance: `S × m = 2,048` blocks = 512 MiB — any run
  of ≤ 2,048 consecutive data blocks touches each stripe at most
  `m = 4` times (Section 3.3).
- Writer memory: `S × m` accumulators = 512 MiB plus per-block CRCs.

Mapping ordinal `o = 100,000` with a covering epoch descriptor
`epoch_id = 1, range = [65,536, 131,072)`: `o_in_epoch = 34,464`;
`stripe = 34464 mod 512 = 160`;
`data_index = 34464 / 512 = 67`. Inverse check:
`65,536 + 67×512 + 160 = 100,000`. ✓

### A.2. Sidecar Index Layout at the Default Geometry

For a full epoch (`real_data_shard_count = 65,536`) at 256 KiB blocks, the
index stream is `2,048 × 16 = 32,768` bytes of parity entries followed by
`65,536 × 8 = 524,288` bytes of data-CRC entries. Running Section 9.4:

- `limit = 262,144 − 8 = 262,136`.
- Block 0: entries start at 0xB8 (184). All parity entries fit
  (184 + 32,768 = 32,952), followed by
  `(262,136 − 32,952) / 8 = 28,648` data-CRC entries, ending exactly at
  the limit. `inline_index_entry_bytes = 262,136 − 184 = 261,952`.
- Spill block 1: `262,136 / 8 = 32,767` data-CRC entries.
- Spill block 2: the remaining `65,536 − 28,648 − 32,767 = 4,121` entries
  (32,968 bytes), zero-filled below its trailing CRC.

So `H = 3`, and the sidecar tape file is
`2H + P + 1 = 6 + 2,048 + 1 = 2,055` blocks (≈ 513.75 MiB).

### A.3. The Canonical Digest Vector

The Section 7.3 map encodes as deterministic CBOR:

```text
83                          array(3)
  87                        array(7)    — tape file 0
    00                      0           tape_file_number
    02                      2           kind = Bootstrap
    01                      1           block_count
    f6 f6 f6 f6             null ×4     ordinal/range/epoch: not applicable
  87                        array(7)    — tape file 1
    01 00 03                1, 0(Object), 3
    00                      0           first_parity_data_ordinal
    f6 f6 f6                null ×3
  87                        array(7)    — tape file 2
    02 01 02                2, 1(ParitySidecar), 2
    f6                      null        first ordinal: not applicable
    00 03                   range [0, 3)
    07                      epoch_id 7
```

SHA-256 of these 25 bytes is
`548ca6c967073a6c1ad011d10fc132c2739e251d015ea45a628bbec96892c26b`.

### A.4. A Minimal Tape, End to End

A smallest-useful tape at a test geometry (k = 2, m = 2, S = 2, 4 KiB
blocks; `E = 4` ordinals per epoch):

```text
file 0   bootstrap        1 block    sequence 0, scheme record, digest of the projected map
file 1   object           4 blocks   ordinals 0..3 — one full epoch
file 2   parity sidecar   2H+4+1     epoch 0, range [0,4), P = S×m = 4
file 3   bootstrap        1 block    sequence 1, is_final_map = true, inline directory
EOD
```

A Scanner finding only this tape: reads file 0 at BOT (bootstrap, tape
UUID, scheme); walks files 1–3 by filemark spacing, classifying file 2 by
its derived-magic header (or its footer, if block 0 of the file is
damaged); applies the final bootstrap's inline directory and validates the
canonical digest over all four entries. A Recoverer asked for
`(file 1, block 2)`: ordinal 2 → epoch 0, stripe 0, data_index 1; gathers
peers (ordinals 0 and the two parity shards of stripe 0), reconstructs, and
verifies the result against the sidecar's data CRC before release.

## Appendix B. Design Rationale (Informative)

This appendix records the reasoning behind non-obvious decisions, so future
revisions do not silently reverse them.

### B.1. Parity lives in separate tape files

No parity byte ever appears inside an object tape file. Objects stay
contiguous and clean — a tar-based payload remains extractable with `mt` +
`tar` alone — and the parity geometry stays independent of object
boundaries. The cost, sidecars consuming their own tape files and
filemarks, is small at archival object sizes.

### B.2. The interleave (data and parity)

Tape damage is overwhelmingly contiguous (scratches, wraps, edge damage).
Mapping consecutive *data* ordinals to consecutive *stripes* (Section 3.3) —
rather than filling one stripe at a time — converts a contiguous burn of up to
`S × m` blocks into at most `m` losses per stripe, exactly the code's tolerance.
The alternative (stripe-fill order) would concentrate a burst into few stripes
and lose data at a fraction of the tolerance.

The parity region carries the same interleave. Parity shards are stored
**parity-index-major** (Section 9.1): the physical block for `(stripe,
parity_index)` is `H + parity_index·S + stripe`, so consecutive parity blocks
belong to consecutive stripes, and a burst inside the parity region loses at
most one parity shard per stripe per `S` blocks traversed. This is what makes
the guarantee hold across the **data→sidecar boundary**, not only within object
data. Worst case, one full-epoch object at default geometry (`k=128, m=4, S=512`,
`H=3`): a stripe's last data shard sits at LBA `(k−1)·S + s`; its parity shards
at LBA `E + 1 + H + j·S + s` (the `+1` is the terminating filemark), spaced `S`
apart and phase-shifted from the data lattice by `1 + H`. Destroying a stripe
requires erasing `m + 1` of its shards; the shortest contiguous span covering
its data shard and `m` parity shards is

```text
span = (m − 1)·S + (S + 1 + H) + 1 = m·S + H + 2 = 2053 blocks ≈ 513 MiB.
```

That is *longer* than the interior data-only worst case (`m·S + 1 = 2049`
blocks; a run of `m·S + 1` consecutive data blocks lands `m + 1` shards in one
stripe), so after this ordering the interior data region — not the boundary — is
the binding constraint, at exactly `m·S = 2048` blocks = 512 MiB. The `1 + H`
phase offset raises the boundary threshold; it never subtracts from the
guarantee.

**Short-epoch residual.** A short final or checkpoint epoch with `R < S` real
data blocks (Section 6.4 implicit zeros) still writes the full `S × m` parity
region, but its `R` data shards occupy LBA `0 .. R−1`, adjacent to the parity
region rather than a full `S·k` run away. Destroying stripe `s < R` (its single
real data shard at LBA `s` plus its `m` parity shards) needs only

```text
span = (m − 1)·S + R + H + 2   (≈ 385 MiB at R = 1, m = 4).
```

Losing that stripe's one real shard and all `m` parity leaves `k − 1` survivors
(the rest implicit zeros), below the `k` needed. So an epoch closing fewer than
`S` data blocks has boundary tolerance `≈ (m − 1)·S + R`, floor `≈ (m − 1)·S`
(~384 MiB) — far above any realistic single media defect and ~380× better than
the pre-`v1.2` stripe-major parity layout, but below the `m·S` headline. Using a
per-epoch stripe count in the locator instead of the constant `S` would be
*worse*, not better: it would re-cluster a short epoch's parity into `m` adjacent
blocks and collapse tolerance to `≈ m` blocks. The constant-`S` locator is both
correct and maximally robust.

### B.3. Implicit zeros instead of padding blocks

A short epoch is closed by *declaring* the missing logical positions
all-zero rather than writing padding blocks. Tape capacity is never spent
on filler; the sidecar's `real_data_shard_count` tells readers which
positions are implicit; and the parity arithmetic is unaffected because
all-zero shards contribute nothing to any accumulator.

### B.4. Derived magics

Sidecar and parity_map magics are HMAC(tape_uuid, role label) so that a
block can be attributed to *this tape* and *this role* without any further
context — stale blocks from a recycled tape, or blocks from another tape in
a mixed pile, fail the magic check immediately. The bootstrap's magic must
stay fixed: it is the entry point read before the UUID is known. Derived
magics are identity, not security (Section 16.1).

### B.5. The bootstrap endianness mix is frozen

The bootstrap header mixes big-endian integers with a little-endian length
and CRCs (Section 8.1). It looks like an accident; it is recorded here
precisely because "normalizing" it would break every existing tape. All
other structures are uniformly little-endian.

### B.6. The canonical digest excludes positions, hashes, and health

Three exclusion classes keep the digest non-circular and stable
(Section 7.3): physical positions would change as control files are
emitted; content hashes of control files would make the digest depend on
bytes whose own validation depends on the digest; and copy-health flags
would mutate the digest at *read* time, invalidating the map by the act of
discovering damage. The digest covers structure only — which is exactly
what recovery needs to be fenced by.

### B.7. parity_map integrity is payload SHA-256 plus a dual copy

A parity_map has no per-block CRCs: the structure is small, dual-copied,
footer-located, and whole-payload hash-verified. Per-block CRCs would
additionally enable splicing a payload from two part-damaged copies; that
corner case was deliberately traded for a simpler layout. A future revision
adding splice recovery is a layout change (new schema version).

### B.8. No per-file commit marker

Commit state lives off tape (Section 3.4). A *per-file* marker would have to
be written after the data it marks — adding a write and a failure mode to
every file — and would still be unreadable exactly when it matters (torn
writes). Version 1.0 instead attests at **barrier** grain: each checkpoint
bootstrap is a batched structural attestation, written after everything it
covers and integrity-bound to the covered structure by its SHA-256 digest
record (Section 7.4 — self-consistency, not authentication, Section 16.1),
amortized across the batch (Section 11.1). A torn tail
is still beyond the durable boundary — invisible to recovery, physically
superseded on resume. What remains out of scope for bare-tape recovery is
only the *commitment* of files past the newest attestation; Section 12.6
classifies that tail, and the Writer's checkpoint cadence bounds it.

### B.9. Content-blind classification

The Scanner classifies object files by elimination, never by reading object
bytes (Section 12.3). This is what payload independence means physically: a
tape of foreign objects is mappable by any conformant implementation, an
unreadable object head block cannot derail the walk, and object formats
need no registered magics with this layer.

### B.10. At most one open epoch

The bounded-restart rule (Section 11.2) caps unprotected ordinals below
`S × k` at every object boundary, so a Resumer rebuilds at most one open epoch
by re-reading at most `S × k − 1` blocks (16 GiB at the default geometry).
Without it, resume cost would grow with the number of epochs left open —
unbounded re-read of a tape that was supposedly fine.

### B.11. Sidecar metadata is replicated head and tail, with a locator

The header/index copy is written before the parity shards *and* after them,
with a footer locator at the very end. Contiguous damage at either end of
the sidecar file leaves a survivable copy at the other; the footer makes
the tail copy findable without trusting block arithmetic; and the epoch
directory makes it findable even with the footer gone (Section 13.3). The
canonical metadata hash is copy-independent, so any surviving copy is
verifiable against any directory entry.

### B.12. The reference off-tape journal is not a media format

The reference implementation's internal tape-file journal is version 3.
Version 3 adds a `checkpointed_through` watermark record so replay can
discard physically written but uncheckpointed orphan bundles and truncate
them before append. This journal version is deliberately not recorded on
tape and does not change any REM-PARITY media byte.

## Appendix C. Open Items Before Freeze (Informative)

1. **Pinned-at-generation image vectors** (Section 17). The byte-level tape
   images and their digest chains must be generated, independently
   re-derived by a second implementation, and frozen into the test-vector
   distribution (Section 18 criterion 2).
2. **Descriptor-format sense classification.** Section 3.5 requires
   filemark/EOD boundary classification for both fixed- and
   descriptor-format SCSI sense; an implementation that classifies
   filemarks from fixed-format sense only does not yet meet it, and closing
   that gap is a freeze item.
3. **The last-resort full filemark-walk scan** (Section 8.4 step 5) is a
   SHOULD-offer whose operational parameters (geometry hints, abort
   conditions, progress reporting) are not yet specified.
4. **Bootstrap re-typing promotion.** The selection rule among
   structurally discovered parity_map files is specified in Section 12.4;
   it needs a multi-parity_map damage-matrix image vector (ranking,
   `tape_file_number` tiebreak, identical-key report, overlay-then-digest),
   pinned-at-generation and second-implementation re-derived before freeze
   (Section 18 criterion 2). Bootstrap re-typing is implemented at SHOULD
   strength with a damage-matrix vector; promotion to MUST, if desired before
   freeze, remains a specification policy decision rather than an
   implementation gap.
5. **Throughput program.** Accelerated GF(2⁸) and CRC kernels must land and
   be proven byte-identical via the Section 17 vectors (Section 18
   criterion 6) before freeze, so the conformance anchor is generated at
   production speed and layout.
6. **Key-30 recovery tooling.** Bootstrap object rows (Section 8.2.1) need a
   scanner/recovery reader that validates each row against the
   recovered filemark map and emits a catalog-less recovery report for both
   plaintext and encrypted RAO objects, demonstrated before format freeze.

## Appendix D. Revision History (Informative)

- **2026-06-11 — first draft.** Initial publication baseline, archived with
  release v1.0.0.
- **2026-07-21 — pre-freeze revisions.** Writer-legal block sizes closed
  over the discovery-candidate set (Section 8.4); object identity row keys
  clarified (Section 8.2.1); epochs redefined as explicit ordinal ranges
  with bare-counter ids (Sections 3.3, 10.5) and short epochs legalized at
  any checkpoint boundary with `FINAL_PARTIAL_EPOCH` reserved for terminal
  `finish()` (Sections 10.5, 11.2); the bootstrap directory ceiling made an
  admission-time refusal with mandatory headroom and seal-at-ceiling
  (Section 8.2.1); reference journal watermark note (Appendix B.12).
- **2026-07-22 — tape-alone recovery claims.** Ordered persistence and the
  synchronizing barrier made normative requirements on the tape I/O layer
  (Section 3.5); commit discipline extended with batched deferred
  synchronization and staged-record semantics (Sections 3.4, 11.1); the
  attested prefix and the bare-tape tail taxonomy
  (attested / unattested / truncated) specified with salvage rules
  (Section 12.6); Appendix B.8 reframed from "no on-tape commit marker" to
  per-file-marker rationale plus barrier-grain structural attestation.

## Author's Address

The ArchiveTech Project
Website: https://archivetech.org
Email: specs@archivetech.org
Reference implementation: https://github.com/archivetechie/remanence
