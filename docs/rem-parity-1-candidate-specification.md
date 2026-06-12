# REM-PARITY-1

## Candidate Specification — the Layer 3c on-tape parity format family

| | |
| --- | --- |
| Status | Candidate (draft for review) |
| Revision | 1 |
| Date | 2026-06-10 |
| Structures | bootstrap block, parity sidecar tape file, parity_map tape file, sidecar epoch directory, filemark-map canonical digest |
| Erasure scheme | `rs-cauchy-gf256-v1` |
| parity_map format id | `rem-parity-map-v1` |
| Reference implementation | Remanence (`crates/remanence-parity`) |
| Supersedes | `layer3c-design-v0.7.2.md` §4–§8 byte tables and reader/writer procedure text (wire format and conformance rules); the v0.7.2 document remains the design-rationale record |
| Companion | `rem-tar-v1-candidate-specification.md` (the object body format these structures protect) |

> **Drafting note (remove at freeze):** this document is the normative
> fixed point; the implementation is validated against it, not the
> reverse. Wire layouts follow what the reference implementation emits
> as of the 2026-06-10 working tree — which includes the post-review
> fixes for the scan unreadable-head rule, the sidecar footer→primary
> fallback, and the resume directory carry — and the v0.7.2 design
> tables, which were never implemented as written, are superseded by
> Appendix A's adjudications. Behavioral requirements deliberately
> exceed the implementation where it is still weak; Appendix B is the
> conformance backlog and closing it is a freeze criterion.

## Abstract

This document specifies the on-tape structures by which Remanence
protects object data on linear tape against media damage and makes a
bare tape self-describing: a fixed **bootstrap block** written at the
beginning of tape and at writer-chosen positions; **parity sidecar
tape files** carrying Reed–Solomon parity and per-block CRCs for each
parity epoch; an optional **parity_map tape file** carrying the sidecar
epoch directory when it outgrows the bootstrap; and the **canonical
filemark-map digest** that chains them together. Parity is computed
over GF(2⁸) with a Cauchy generator (`rs-cauchy-gf256-v1`),
incrementally accumulated as data streams, and written strictly in
separate tape files so every object archive remains a clean tar file.
The format is designed for catalog-less recovery: a reader holding
only this document, a damaged tape, and the tape's UUID-independent
entry point (the bootstrap) can reconstruct the tape's structure,
verify it cryptographically, and recover up to *m* damaged blocks per
stripe — including the case where the damaged block is the first block
of the very file that describes it.

## Table of Contents

1. [Introduction](#1-introduction)
2. [Conventions and Terminology](#2-conventions-and-terminology)
3. [Tape Model and Address Spaces](#3-tape-model-and-address-spaces)
4. [Common Primitives](#4-common-primitives)
5. [The Erasure Scheme rs-cauchy-gf256-v1](#5-the-erasure-scheme-rs-cauchy-gf256-v1)
6. [The Filemark Map and Canonical Digest](#6-the-filemark-map-and-canonical-digest)
7. [The Bootstrap Block](#7-the-bootstrap-block)
8. [The Parity Sidecar Tape File](#8-the-parity-sidecar-tape-file)
9. [The parity_map Tape File and the Epoch Directory](#9-the-parity_map-tape-file-and-the-epoch-directory)
10. [Writer Obligations](#10-writer-obligations)
11. [Scanner Obligations](#11-scanner-obligations)
12. [Recoverer Obligations](#12-recoverer-obligations)
13. [Resumer Obligations](#13-resumer-obligations)
14. [Errors](#14-errors)
15. [Security Considerations](#15-security-considerations)
16. [Test Vector Requirements](#16-test-vector-requirements)
17. [Candidate Freeze Criteria](#17-candidate-freeze-criteria)
18. [References](#18-references)

Appendix A. [Decisions and Superseded Material](#appendix-a-decisions-and-superseded-material)
Appendix B. [Reference Implementation Gaps](#appendix-b-reference-implementation-gaps)

---

## 1. Introduction

### 1.1. Purpose and Design Goals

1. **Catalog-less recovery.** Everything needed to map, verify, and
   repair a tape lives on the tape. Local state (catalog, journal)
   accelerates but is never required.
2. **Clean coexistence with standard tooling.** Parity lives in
   separate, filemark-delimited tape files; object archives remain
   plain tar files navigable with `mt`/`tar` (REM-TAR-1 §15). No
   parity bytes ever appear inside an object.
3. **Bounded damage tolerance with bounded memory.** Default geometry
   tolerates ~512 MiB of contiguous damage per epoch while the writer
   holds only `S × m` parity accumulators (~512 MiB), regardless of
   object sizes.
4. **No circular failure.** The structures that describe the tape are
   themselves replicated and discoverable such that single-block
   damage to any one of them — including the first block of a file —
   never makes an unrelated epoch unrecoverable (§11.5, §12.3).
5. **Fail-closed durability.** A tape file either completed its
   blocks, its synchronous filemark, and its off-tape commit record,
   or it does not exist for recovery purposes (§3.4, §10.1).
6. **Long-term recoverability.** A future implementer holding only
   this document and its static test vectors can read every conformant
   tape; every cryptographic and arithmetic primitive is fully
   parameterized here.

### 1.2. Relationship to Adjacent Layers

- **REM-TAR-1** defines the object bodies occupying object tape files;
  this format treats them as opaque fixed blocks.
- **Layer 3a** provides fixed-block tape I/O, filemarks, positioning,
  and sense classification; §3.5 states what this format requires of
  it.
- **The tape-file journal and Layer 4 catalog** are local, off-tape
  records. Their formats are out of scope; §10.1 defines the abstract
  *commit record* they implement, and §13 defines how a Resumer uses
  the committed prefix they describe.
- **Drive compression MUST be off** for parity tapes (§10.4): block
  bytes must map 1:1 to media so damage geometry and parity coverage
  correspond.

### 1.3. Non-Goals

This format performs no encryption and no authentication (§15.1), does
not define capacity or placement *policy* (only the policy's wire
consequences), does not define the journal/audit/catalog formats, and
does not support multiple partitions (all positions are partition 0).

## 2. Conventions and Terminology

### 2.1. Requirements Language

BCP 14 [RFC2119] [RFC8174]: "MUST", "SHOULD", "MAY" etc. are normative
only in all capitals.

### 2.2. Conformance Targets

- **Writer**: produces parity-protected tapes (§10).
- **Scanner**: reconstructs the filemark map from a bare tape (§11).
- **Recoverer**: reconstructs damaged data blocks from parity (§12).
- **Resumer**: re-opens a committed tape for append (§13).
- **Verifier**: validates a tape's structures and digests end to end
  without recovering payload (subset of §11 + §12 checks; reports all
  nonconformities).

### 2.3. Definitions

- **Tape file**: a run of fixed blocks delimited by exactly one
  trailing filemark. Kinds: **object**, **parity sidecar**,
  **bootstrap**, **parity_map**.
- **Epoch** (code: "neighborhood"): one parity protection unit of
  `S × k` data ordinals; epoch id = `ordinal / (S × k)`.
- **Stripe**: one RS codeword: `k` data shards + `m` parity shards,
  each one full block.
- **Watermark `W`** (`highest_protected_ordinal`): ordinals `< W` have
  emitted sidecars. **Total `T`** (`map_total_data_ordinals`): ordinals
  `< T` exist as committed data. Always `W ≤ T`.
- **Committed**: inside the durable boundary (§3.4).
- **Block size**: the tape's fixed block size; one value per tape,
  recorded in the bootstrap. All structures here are sized in blocks
  of this one size.

### 2.4. Integer and Byte Conventions

All multi-byte integers in sidecar and parity_map structures are
**little-endian**. The bootstrap header mixes endianness per field —
its table (§7.1) is authoritative; the mix is deliberate and frozen
(Appendix A.1). All offsets are zero-based. Arithmetic on values read
from tape MUST be checked; overflow is rejection, never wraparound.

### 2.5. Constants

| Constant | Value |
| --- | --- |
| `BOOTSTRAP_MAGIC` | `52 45 4D 00 42 4F 4F 01` (`"REM\0BOO\x01"`, fixed bytes) |
| `BOOTSTRAP_SCHEMA_MAJOR` / `MINOR` | 1 / 1 |
| `BOOTSTRAP_HEADER_LEN` | 0x34 |
| `FLAG_NO_PARITY` | bit 0 of bootstrap flags |
| `MAX_BOOTSTRAP_SCAN_BLOCKS` | 1024 |
| Default candidate block sizes | 256 KiB, 512 KiB, 1 MiB |
| `SIDECAR_MAGIC_LABEL` | `"REM\0PAR\x01"` |
| `SIDECAR_FOOTER_MAGIC_LABEL` | `"REM\0PARFOOT\x01"` |
| `PARITY_MAP_MAGIC_LABEL` | `"REM\0PMAP\x01"` |
| `PARITY_MAP_FOOTER_MAGIC_LABEL` | `"REM\0PMAPFOOT\x01"` (Appendix A.4) |
| `SIDECAR_SCHEMA_VERSION` | 1 |
| `SIDECAR_HEADER_LEN` / CRC offset | 0xB8 / 0xB0 |
| `SIDECAR_FOOTER_LEN` / CRC offset | 0x80 / 0x78 |
| `PARITY_INDEX_ENTRY_LEN` / `DATA_CRC_ENTRY_LEN` | 16 / 8 |
| `PARITY_MAP_FORMAT_ID` | `"rem-parity-map-v1"` |
| `PARITY_MAP_HEADER_LEN` = footer len / CRC offsets | 0xB8 / 0xB0 |
| `SCHEME_ID` | `"rs-cauchy-gf256-v1"` |
| Default scheme | k=128, m=4, S=512 @ 256 KiB blocks |
| Canonical-hash domain | `"remanence-sidecar-metadata-v1"` |
| Minimum block sizes | bootstrap frame 0x3C; parity_map 0xB8; sidecar 0xC0 |

## 3. Tape Model and Address Spaces

### 3.1. Tape Files

A parity tape is a sequence of tape files numbered densely from 0 at
BOT, each terminated by exactly one filemark, followed by EOD:

```text
| bootstrap(0) | object | object | sidecar | object | ... | bootstrap(final) | EOD
```

Tape file 0 MUST be a bootstrap. The format owns every filemark; an
object body (REM-TAR-1) MUST NOT write filemarks of its own. A tape
file MUST contain at least one block (an immediate filemark is
structural damage).

### 3.2. Address Spaces

- **`TapeFilePosition`** = `(tape_file_number: u32, block_within_file:
  u64)`.
- **Physical LBA** (partition 0):
  `LBA(f, b) = Σ_{g<f}(block_count(g) + 1) + b` (each prior file
  contributes its blocks plus one filemark). The append point after a
  committed prefix is `Σ(block_count + 1)` over all committed files.
- **`ParityDataOrdinal`** (u64): dense numbering of object data blocks
  only, in tape order, skipping filemarks and all non-object files.
  For an object file with first ordinal `F`,
  `ordinal(b) = F + b`. Non-object files have no ordinals; parity
  shards have no ordinals.
- **`BodyLba`** (REM-TAR-1): per-object block index;
  `(tape_file_number, body_lba)` resolves to an ordinal through the
  filemark map.

### 3.3. Ordinal ↔ Stripe Mapping

For scheme `(k, m, S)`, with `E = S × k` data ordinals per epoch:

```text
epoch       = o / E
o_in_epoch  = o mod E
stripe      = o_in_epoch mod S        (stripe index varies fastest)
data_index  = o_in_epoch / S          (0 ≤ data_index < k)
inverse:  o = epoch·E + data_index·S + stripe
```

The interleave is load-bearing: `N ≤ S` physically consecutive blocks
land in `N` distinct stripes, so contiguous damage of up to `S × m`
blocks stays within the per-stripe tolerance `m`. Parity shards are
addressed `(epoch, stripe, parity_index)` with `0 ≤ parity_index < m`.

### 3.4. The Durable Boundary

A tape file is **committed** only when all of the following completed,
in order: its blocks; its synchronous trailing filemark; and a durable
off-tape **commit record** (the reference implementation's journal
bundle). There is no on-tape commit marker and no on-tape "unclean"
marker: an interrupted tail simply lies beyond the last committed file
and is physically superseded on resume (§13). Tape files begin and
commit strictly sequentially (next = last committed + 1, first = 0,
at most one in-flight). Readers seeded from a *prefix*-scoped map
(§6.4) MUST treat suffix rows as forensic only — never recovery
inputs.

### 3.5. Requirements on the I/O Layer

Fixed-block reads/writes only; a read returning other than exactly one
block is an error, with two classified boundary outcomes: **Filemark**
and **EndOfData**. Boundary classification MUST work for both fixed-
and descriptor-format SCSI sense (Appendix B item 7 — the reference
implementation classifies filemarks from fixed-format sense only).
Writers/readers MAY track position by +1-per-block dead reckoning but
MUST resync via READ POSITION after any boundary or unclassified
error.

## 4. Common Primitives

### 4.1. CRC-64/XZ

All CRCs in this format are CRC-64/XZ: polynomial
`0x42F0E1EBA9EA3693`, reflected input and output (reflected polynomial
constant `0xC96C5795D7870F42`), init `0xFFFF_FFFF_FFFF_FFFF`, xorout
`0xFFFF_FFFF_FFFF_FFFF`. Stored little-endian. Normative vectors:

```text
crc64("123456789")        = 0x995DC9BBDF1939FA   (LE bytes fa 39 19 df bb c9 5d 99)
crc64("")                 = 0
crc64([0x00])             = 0x1FADA17364673F59
crc64([0xFF])             = 0xFF00000000000000
crc64(0x00 × 262144)      = 0x261BDF3D299838FC
crc64(0xFF × 262144)      = 0x55433DD0F38908BA
```

### 4.2. HMAC-Derived Magics

Sidecar and parity_map blocks carry **per-tape** magics:

```text
magic = HMAC-SHA-256(key = tape_uuid[16], msg = LABEL)[0..8]
```

with the labels of §2.5. The label bytes never appear on tape. Each
block role has a distinct label (sidecar header vs footer; parity_map
header vs footer — Appendix A.4). The bootstrap magic alone is a fixed
byte string, because the reader does not yet know the tape UUID
(§7.1). Derived magics are an *identity* mechanism (these blocks
belong to this tape and role), not authentication (§15.1).

### 4.3. CBOR

All CBOR payloads in this format are single definite-length,
integer-keyed maps in RFC 8949 deterministic encoding (shortest-form
integers, sorted keys, no tags/floats/indefinite items). Decoders MUST
ignore unknown integer keys at every map level (the 1.x extension
mechanism) and MUST reject duplicate keys and non-canonical encoding.

## 5. The Erasure Scheme rs-cauchy-gf256-v1

### 5.1. Field and Generator

GF(2⁸) with reduction polynomial **0x11D** (x⁸+x⁴+x³+x²+1).
Inversion: `inv(v) = v^254`; `inv(0)` is an error. The generator is
the `m × k` Cauchy matrix with the contiguous seed partition:

```text
X_j = k + j   (j in 0..m)      Y_i = i   (i in 0..k)
G[j][i] = inv(X_j XOR Y_i)               requires k + m ≤ 255
```

### 5.2. Encoding

Systematic, byte-wise across full blocks:

```text
parity_j = XOR over i in 0..k of  G[j][i] ⊗ data_i
```

where `⊗` is GF(2⁸) scalar-by-block multiplication. Encoding MUST be
expressible as order-independent incremental accumulation
(`accumulate(i, shard)` into `m` zeroed accumulators), and incremental
and batch encodings MUST be byte-identical.

**Implicit zeros:** in a final partial epoch, data positions beyond
the real data are all-zero shards that are *never written to tape* and
never accumulated; the sidecar's `real_data_shard_count` vs
`logical_shard_count = S × k` tells readers which positions are
implicit. Implicit-zero positions are never erasures.

### 5.3. Reconstruction

Given any `k` of the `k + m` shards of a stripe (data shards first in
index order, then parity), form the survivor rows of `[I_k ; G]`,
invert by Gauss–Jordan over GF(2⁸), recover the data shards, and
re-encode any missing parity. Fewer than `k` survivors is
unrecoverable. Maximum tolerated erasures per stripe: `m`.

### 5.4. Scheme Parameters

`k ≥ 2`, `1 ≤ m ≤ k`, `S ≥ 1`, `k + m ≤ 255`,
`S × (k + m) ≤ 2³² − 1`. Defaults (informative): k=128, m=4 with `S`
chosen as `max(1, ceil(target / (block_size × m)))` for a ~512 MiB
contiguous-damage target — S=512 at 256 KiB blocks (epoch =
67,584 blocks total; 65,536 data; tolerance 2,048 blocks). A
conservative profile k=64, m=6, target 384 MiB is also defined. The
scheme triple is recorded in the bootstrap and every sidecar; readers
MUST use the recorded values, never defaults.

### 5.5. Normative Vectors (Appendix A of the design, pinned)

```text
gf_inv(0x02) = 0x8E      gf_inv(0x03) = 0xF4
k=2, m=2 ⇒ G = [[0x8E, 0xF4], [0xF4, 0x8E]]
data d0 = 01 02 03 04, d1 = 10 20 30 40
parity p0 = 75 EA 9F C9,  p1 = FC E5 19 D7
```

### 5.6. Per-Shard CRCs

`data_shard_crc64` = CRC-64/XZ over the entire fixed data block as
written through the parity path. `parity_shard_crc64` = CRC-64/XZ over
the entire raw parity shard block. Both are recorded in the sidecar
index (§8.3); they are the verify-before-trust and
verify-after-reconstruction anchors (§12.4).

## 6. The Filemark Map and Canonical Digest

### 6.1. Entries

The filemark map is the tape's structural table of contents — one
entry per tape file:

| Field | Type | Applies to |
| --- | --- | --- |
| `tape_file_number` | u32, dense from 0 | all |
| `kind` | Object=0, ParitySidecar=1, Bootstrap=2, ParityMap=3 | all |
| `block_count` | u64 ≠ 0 (data blocks, excluding the filemark; bootstrap MUST be 1) | all |
| `first_parity_data_ordinal` | u64 | objects only |
| `protected_ordinal_start` / `protected_ordinal_end_exclusive` | u64 half-open | sidecars only |
| `epoch_id` | u64 | sidecars only |

Validity: dense numbering from 0; object first-ordinals dense and
contiguous from 0; kind-specific fields exclusive to their kinds.
`T = max(first + block_count)` over objects;
`W = max(end_exclusive)` over sidecars (0 if none).

### 6.2. Canonical Digest

The digest is SHA-256 over the deterministic CBOR encoding of an array
of per-entry 7-element arrays, ascending by tape file number:

```text
[tape_file_number, kind_code, block_count,
 first_parity_data_ordinal | null,
 protected_ordinal_start   | null,
 protected_ordinal_end_exclusive | null,
 epoch_id | null]
```

**Exclusions (the non-circularity rule):** no physical position hints,
no content hashes, no bootstrap/parity_map payload bytes, and no copy
health. This is what lets a bootstrap's digest cover the map
*including the bootstrap's own structural entry* — the digest of the
projected map is computed before the bootstrap is written, and the
written bootstrap does not change the projection. Pinned vector: the
map `[bootstrap(#0, 1 blk), object(#1, 3 blk, first ordinal 0),
sidecar(#2, 2 blk, epoch 7, range [0,3))]` projects to hex
`8387000201f6f6f6f68701000300f6f6f687020102f6000307` with digest
`548ca6c967073a6c1ad011d10fc132c2739e251d015ea45a628bbec96892c26b`.

### 6.3. The Digest Record

Wherever stored (bootstrap field 2, parity_map payload/locators), the
digest travels with its scope: `tape_file_count` (the prefix length it
covers), `map_total_data_ordinals` (T over that prefix),
`highest_protected_ordinal` (W over that prefix), and `is_final_map`.
Validation MUST recompute the digest over exactly the leading
`tape_file_count` entries and cross-check all three scalars.

### 6.4. Scope

A final digest yields a **Complete** map; a non-final digest yields a
**Prefix** map valid for exactly `tape_file_count` files. Recovery
MUST be fenced to the validated scope (§12.2).

## 7. The Bootstrap Block

### 7.1. Fixed Frame (one block, exactly)

| Offset | Len | Field | Type | Constraint |
| --- | ---: | --- | --- | --- |
| 0x00 | 8 | magic | fixed bytes | `BOOTSTRAP_MAGIC` |
| 0x08 | 2 | schema_major | **u16 BE** | MUST be 1; readers reject ≠ 1 |
| 0x0A | 2 | schema_minor | **u16 BE** | readers accept any |
| 0x0C | 4 | flags | **u32 BE** | bit 0 = no-parity; other bits MUST be 0 |
| 0x10 | 16 | tape_uuid | raw | the tape's identity; HMAC key for §4.2 |
| 0x20 | 4 | block_size_bytes | **u32 BE** | MUST equal the size of the block it was read with |
| 0x24 | 4 | sequence | **u32 BE** | 0 at BOT; monotonic per copy |
| 0x28 | 4 | cbor_payload_len | **u32 LE** | |
| 0x2C | 8 | crc64_header | **u64 LE** | CRC-64/XZ over bytes 0x00..0x2C |
| 0x34 | var | CBOR payload | §7.2 | |
| +len | 8 | crc64_payload | **u64 LE** | CRC-64/XZ over the payload bytes |
| … | | zero fill to block end | | readers ignore |

The endianness mix (BE header integers, LE length+CRCs) is frozen
verbatim — do not "normalize" it (Appendix A.1). Parse order: length ≥
0x3C → magic → header CRC → major → payload bounds (checked) → payload
CRC → CBOR.

### 7.2. CBOR Payload

Integer-keyed map:

| Key | Type | Presence | Meaning |
| ---: | --- | --- | --- |
| 1 | map | required unless no-parity | scheme: `{1: tstr scheme_id, 2: uint k, 3: uint m, 4: uint S}` |
| 2 | map | required unless minimal no-parity | digest record: `{1: bytes32 sha, 2: uint tape_file_count, 3: uint total_ordinals, 4: uint highest_protected, 5: bool is_final_map}` — all five required when present |
| 3 | tstr | optional | writer version |
| 4 | tstr | optional | RFC 3339 write timestamp |
| 5 | bool | written always; absent ⇒ false | `drive_compression` — effective hardware compression at session open. `true` on a parity bootstrap MUST be rejected (§10.4) |
| 20 | map | optional | inline sidecar epoch directory (§9.4) |
| 21 | map | optional | `ParityMapReference` (§9.5) |

Keys 20 and 21 are mutually exclusive; both present is a parse error.
A **no-parity bootstrap** (flag bit 0) may omit everything except the
fixed frame; readers MUST NOT require scheme/digest on it. Unknown
keys are ignored at every level.

### 7.3. Placement (Writer)

Mandatory: sequence 0 at BOT (tape file 0) and a final bootstrap
(`is_final_map = true`) at `finish()`. Between them, placement is
content-driven policy (bundles/ordinals floors with an EOM taper and a
minimum physical separation), evaluated only at object boundaries —
parameters are operator-tunable and not normative; their *validity*
rules are (non-zero floors; taper fractions in (0,1], descending, with
strictly increasing divisors). Checkpoint bootstraps (§10.3) carry
`is_final_map = false` and a prefix-scoped digest. Sequence numbers
strictly increase across all copies.

### 7.4. Discovery (Reader)

A Scanner with no catalog MUST attempt, in order, until a bootstrap is
found:

1. **BOT** (LBA 0) — always.
2. **Hint positions** supplied out of band (catalog, journal, MAM,
   operator).
3. **Heuristic fractional probing** MAY be used (the reference
   implementation probes the 5%…95% marks of a total-size hint).
4. **A bounded forward scan** from each candidate position of up to
   `MAX_BOOTSTRAP_SCAN_BLOCKS` blocks.
5. As a last resort, an implementation SHOULD offer an explicit
   opt-in full filemark-walk scan (Appendix B item 8).

When the block size is unknown, candidates (§2.5) MUST each be applied
as a real drive reconfiguration before reading; a parsed bootstrap is
accepted only if its `block_size_bytes` equals the configured read
size. Per-block scan rules: magic miss → next block; parse failure →
keep scanning; **medium-error read (sense key 0x03, either sense
format) → skip the block and continue**; filemark → continue; EOD →
no bootstrap at this position; any other transport error → abort
discovery; `drive_compression=true` on a parity bootstrap → abort
everything. Both the known-size and candidate-size paths MUST apply
the same continue/abort taxonomy (Appendix B item 9).

### 7.5. Authoritative Selection

When multiple bootstraps are found, the authoritative copy is the one
with the lexicographically greatest key
`(is_final_map, sequence, map_total_data_ordinals)` (missing digest ⇒
`(false, seq, 0)`); ties keep the earlier find. The selected digest
record then governs map validation (§11.4).

## 8. The Parity Sidecar Tape File

### 8.1. Structure

One sidecar per parity epoch, in its own tape file of
`total = 2H + P + 1` blocks (`H` = header/index copy blocks,
`P = S × m` parity shard blocks):

```text
blocks 0 .. H−1        primary header/index copy
blocks H .. H+P−1      parity shards, stripe-major: shard i ⇒
                       stripe = i / m, parity_index = i mod m
blocks H+P .. 2H+P−1   tail header/index copy
block  2H+P            footer locator
```

Parity shard blocks are raw blocks (no headers). The tail copy MUST
carry identical metadata and index content to the primary (only
`copy_kind` and recomputed CRCs differ); when both copies parse,
readers MUST verify they agree and reject divergence. Minimum block
size: 0xC0.

### 8.2. Header Block (block 0 of each copy) — all LE

| Offset | Len | Field | Constraint |
| --- | ---: | --- | --- |
| 0x00 | 8 | magic | HMAC(`tape_uuid`, `"REM\0PAR\x01"`)[0..8] |
| 0x08 | 16 | tape_uuid | MUST match |
| 0x18 | 8 | epoch_id u64 | |
| 0x20 | 2 | k u16 | ≠ 0 |
| 0x22 | 2 | m u16 | ≠ 0 |
| 0x24 | 4 | S u32 | ≠ 0 |
| 0x28 | 4 | block_size u32 | MUST equal actual |
| 0x2C | 4 | schema_version u32 | MUST be 1 |
| 0x30 | 8 | protected_ordinal_start u64 | |
| 0x38 | 8 | protected_ordinal_end_exclusive u64 | > start |
| 0x40 | 8 | logical_shard_count u64 | MUST = S × k |
| 0x48 | 8 | real_data_shard_count u64 | = end − start, ≤ logical |
| 0x50 | 4 | parity_block_count u32 | MUST = S × m |
| 0x54 | 4 | data_crc_count u32 | MUST = real_data_shard_count |
| 0x58 | 4 | shard_index_block_count u32 (H) | MUST equal recomputed layout |
| 0x5C | 4 | inline_index_entry_bytes u32 | MUST equal recomputed layout |
| 0x60 | 8 | sidecar_total_block_count u64 | = 2H + P + 1 |
| 0x68 | 8 | primary_header_start_block u64 | MUST = 0 |
| 0x70 | 8 | tail_header_start_block u64 | MUST = H + P |
| 0x78 | 8 | footer_block_index u64 | MUST = 2H + P |
| 0x80 | 2 | copy_kind u16 | 1 = primary, 2 = tail |
| 0x82 | 2 | reserved | MUST be 0 |
| 0x84 | 4 | copy_generation u32 | MUST be 0 in v1 |
| 0x88 | 32 | canonical_metadata_hash | §8.4 |
| 0xA8 | 8 | reserved u64 | MUST be 0 |
| 0xB0 | 8 | header_crc64 | CRC-64/XZ over 0x00..0xB0 |
| 0xB8 | var | inline index entries | §8.3 |
| … | | zero fill | MUST be zero to block_size−8 |
| bs−8 | 8 | block0_crc64 | CRC-64/XZ over bytes 0..block_size−8 |

### 8.3. Index Entry Stream

Packed binary (not CBOR), all parity entries first (stripe-major),
then data-CRC entries in ordinal order:

- **Parity entry (16 B)**: u32 stripe_index; u16 parity_index; u16
  reserved MUST be 0; u64 parity_shard_crc64.
- **Data-CRC entry (8 B)**: u64 data_shard_crc64 — one per *real*
  data shard only (implicit zeros carry no CRC).

The stream begins in block 0 after the header and spills into blocks
1..H−1; **entries never straddle a block boundary**. Each spill block
ends with a u64 LE CRC-64/XZ over its bytes 0..block_size−8; unused
spill space MUST be zero. `H` and `inline_index_entry_bytes` are fully
determined by `(block_size, S, m, real_count)`; readers MUST recompute
and reject mismatches.

### 8.4. Canonical Metadata Hash

`canonical_metadata_hash` = SHA-256 over: the domain string
`"remanence-sidecar-metadata-v1"`, the magic, and every header
metadata field plus every index entry, in header order, integers LE —
**excluding** `copy_kind`, `copy_generation`, and all CRC fields. Both
copies of one sidecar therefore carry the same hash, and the epoch
directory (§9.4) can verify a header copy independently of which copy
survived. Readers MUST verify it on every index parse.

### 8.5. Footer Block (last block) — all LE

| Offset | Len | Field |
| --- | ---: | --- |
| 0x00 | 8 | magic = HMAC(`tape_uuid`, `"REM\0PARFOOT\x01"`)[0..8] |
| 0x08 | 2 | footer_version u16 = 1 |
| 0x0A | 2 + 4 | reserved, MUST be 0 |
| 0x10 | 16 | tape_uuid |
| 0x20 | 8 | epoch_id u64 |
| 0x28 | 8 + 8 | protected ordinal start / end_exclusive |
| 0x38 | 4 | H u32 |
| 0x3C | 4 | P u32 |
| 0x40 | 8 | sidecar_total_block_count u64 |
| 0x48 | 8 | primary_header_start_block = 0 |
| 0x50 | 8 | tail_header_start_block = H + P |
| 0x58 | 32 | canonical_metadata_hash |
| 0x78 | 8 | footer_crc64 over 0x00..0x78 |
| 0x80… | | zero fill, MUST be zero |

The footer is a *locator*, holding everything needed to find and check
either header copy without reading the other.

## 9. The parity_map Tape File and the Epoch Directory

### 9.1. Purpose

The **sidecar epoch directory** is the per-epoch repair root: for each
sidecar it records where it is, what range it protects, its layout
counts, and its `canonical_metadata_hash` — enough to locate and
verify a surviving header copy even when scan-time classification of
that sidecar failed. The directory rides inline in the bootstrap
(key 20) when it fits; otherwise it is written as a separate
**parity_map tape file** referenced from the bootstrap (key 21).

### 9.2. parity_map Structure

`M = ceil((0xB8 + payload_len) / block_size)` blocks per copy:

```text
blocks 0..M−1     primary copy   (header at 0xB8-offset + payload, zero tail)
blocks M..2M−1    tail copy      (identical content; copy_kind differs)
block  2M         footer locator
total = 2M + 1
```

Header and footer blocks (both 0xB8 fixed region + CRC at 0xB0, all
LE) carry: magic (header label `"REM\0PMAP\x01"`; footer label
`"REM\0PMAPFOOT\x01"` — Appendix A.4); schema/footer version = 1;
copy_kind u16 (1/2; footer has reserved); tape_uuid; sequence u32;
block_size u32; payload_len u64; payload_sha256 (32); canonical map
digest (32); the digest scope triple (`tape_file_count` u32,
`total_data_ordinals` u64, `highest_protected_ordinal` u64);
`is_final_directory` u8 ∈ {0,1} + 3 pad bytes (0); M;
total = 2M + 1; and the three locator block indices (0, M, 2M).
Payload bytes continue across the copy's blocks; the copy's unused
tail MUST be zero. There are **no per-block CRCs**: payload integrity
is `payload_sha256`, redundancy is the dual copy (Appendix A.5).

### 9.3. parity_map Payload (CBOR)

`{1: tstr "rem-parity-map-v1", 2: bytes16 tape_uuid, 3: uint sequence,
4: SidecarEpochDirectory, 5: bytes32 canonical_map_digest,
6: ?tstr writer_version, 7: ?tstr write_timestamp}`. The decoded
payload MUST match the header/footer locator fields and hash to
`payload_sha256`.

### 9.4. SidecarEpochDirectory (CBOR)

`{1: uint scope_tape_file_count, 2: uint scope_total_data_ordinals,
3: uint scope_highest_protected_ordinal, 4: bool is_final_directory,
5: [entries]}`, entry =
`{1: uint tape_file_number, 2: uint epoch_id, 3: uint
protected_ordinal_start, 4: uint protected_ordinal_end_exclusive,
5: uint sidecar_total_block_count, 6: uint sidecar_header_block_count,
7: uint parity_shard_block_count, 8: bytes32 canonical_metadata_hash,
9: uint flags}`. Flags: 0x01 final-partial-epoch, 0x02
primary-known-good, 0x04 tail-known-good; unknown bits MUST be
rejected. Invariants: entries strictly ascending by tape file number
and `< scope_tape_file_count`; non-empty ranges; non-zero block
counts; `max(end_exclusive)` over entries (0 if none) MUST equal
`scope_highest_protected_ordinal`.

### 9.5. ParityMapReference (bootstrap key 21)

`{1: uint tape_file_number, 2: uint block_count, 3..5: the scope
triple, 6: bool is_final_directory, 7: bytes32 payload_sha256,
8: bytes32 canonical_map_digest}` — enough to locate the parity_map
file, bound its size, and verify its payload without trusting it.

### 9.6. Inline-vs-External Rule (Writer)

The directory rides inline iff the fully framed bootstrap (base
payload + key 20) fits the block with a production slack margin of
4096 bytes (margin waived below 8 KiB blocks, a test-geometry
allowance). The fit check MUST be the typed too-large signal from a
real framing attempt, never a string match. External path ordering:
the parity_map takes the next tape file number `N`; the bootstrap
takes `N+1`; the directory/digest scope covers `N+2` files (both
control files included); the provisional payload is encoded with a
zeroed digest to fix `M`, the real digest is then computed over the
projected map and re-encoded, and the block count MUST be unchanged.

## 10. Writer Obligations

### 10.1. Commit Discipline (per tape file)

```text
begin (durable boundary, dense numbering, one in flight)
→ write blocks            (any short write / EOM / completion-unknown ⇒ abandon)
→ synchronous filemark    (same failure rule; EOM here ⇒ abandon — never commit)
→ filemark-map push
→ durable-boundary commit
→ [object close only] emit queued sidecars (each its own full cycle)
→ off-tape commit record  (THE commit point)
```

Failure at any step MUST abandon the in-flight file (boundary rolls
back) and **poison the writer**: a poisoned writer refuses every
subsequent operation. A known-completion block-write failure (the
failed block consumed no slot) MAY leave the writer usable; a
completion-unknown failure MUST poison. The watermark `W` advances
only after a sidecar's boundary commit.

### 10.2. Epochs and Sidecars

Parity accumulates incrementally per data block. At `S × k` data
blocks the epoch closes into a **pending** (memory/spool) sidecar; no
tape I/O occurs mid-object. Pending sidecars are emitted as tape files
when the current object closes; the final partial epoch (if any) is
closed at `finish()` with implicit-zero positions and **no padding
blocks written to tape**. After each object's bundle, unprotected
ordinals MUST number fewer than `S × k` (the v1 bounded-restart rule —
at most one open epoch ever needs rebuilding).

### 10.3. checkpoint() and finish()

`checkpoint()` (only between objects; mid-object MUST be refused)
writes a non-final bootstrap with a prefix-scoped digest and commits a
control record: the clean resumable boundary. `finish()` closes the
partial epoch, emits remaining sidecars, writes the final bootstrap
(`is_final_map = true`, full-scope digest, inline directory or
parity_map per §9.6), and commits. A finished tape accepts no further
appends.

### 10.4. Session Preconditions

Drive hardware compression MUST be verified off (set + read back)
before any parity write; the effective value is recorded as bootstrap
key 5, and a parity bootstrap recording `true` MUST be rejected by
readers and writers alike. Capacity admission (early-warning reserve)
is policy, but a writer MUST treat hard EOM mid-file as a commit
failure (§10.1), never as a signal to truncate.

## 11. Scanner Obligations

### 11.1. Inputs and Authority

If a catalog-supplied map is available and validates (tape UUID,
watermark ≤ total, watermark equals the map's sidecar watermark), it
is authoritative and no physical scan occurs. Otherwise the Scanner
walks the tape from LBA 0.

### 11.2. The Walk

Per tape file: read the head block; measure length by filemark
spacing (`space_filemarks(1)`; the file's block count = LBA delta − 1;
a zero-block file or missing trailing filemark is structural damage);
EOD at a file start ends the walk.

### 11.3. Classification Ladder (in order)

1. **Bootstrap**: fixed magic + full parse + payload block size equals
   read size + file is exactly 1 block.
2. **parity_map**: primary header parses (per-tape magic); block count
   MUST equal the header's total (mismatch is a hard error). A Scanner
   SHOULD also probe the parity_map tail copy and footer when the
   primary is unreadable (Appendix B item 4).
3. **Sidecar (primary)**: header parses; block count MUST equal the
   header's total.
4. **Sidecar (footer/tail probe)**: read the last block; if it parses
   as a sidecar footer, the footer's total MUST equal the measured
   block count; then verify the tail header copy against the footer
   (field-for-field); classification MAY fall back to footer fields
   alone if the tail is unreadable.
5. **Object, by elimination** — never by reading object content.

**Unreadable head block:** the Scanner MUST NOT abort. It MUST
measure the file by filemark spacing, run the footer/tail sidecar
probe, and otherwise classify the file as an object candidate. (This
is the no-circular-failure rule: the block needing recovery may be the
block that would have classified the file.)

### 11.4. Overlay and Validation

After the walk, apply the authoritative directory overlay in priority
order: bootstrap inline directory → bootstrap `ParityMapReference`
(read the referenced file, verify payload SHA-256 and all
cross-checks; on any failure fall through) → structural parity_map
overlay → none. The overlay re-types directory-listed files as
sidecars with their epoch/range (block counts MUST match; conflicts
with scanned bootstrap/parity_map kinds are hard errors), renumbers
object ordinals, truncates to the directory scope, and cross-checks
the scope totals. Finally validate the map against the authoritative
bootstrap's digest record (§6.3): recompute over the scoped prefix and
compare digest + all three scalars; mismatch is fatal to the map (not
to the tape — a different bootstrap copy may carry a usable scope).

### 11.5. Epoch Isolation (normative guarantee)

Damage confined to one sidecar's metadata (any or all of: header
copies, footer, its directory entry's flags) MUST NOT degrade
classification, mapping, digest validation, or recovery of any other
epoch. At worst the damaged epoch becomes "metadata unavailable". Copy
health is deliberately excluded from the canonical digest so that
discovering damage never invalidates the map.

## 12. Recoverer Obligations

### 12.1. Inputs

A validated scoped map (§11), the bootstrap scheme, and the failed
addresses (`(tape_file, body_lba)` or ordinals).

### 12.2. Fail Before I/O

Before any tape read, reject: ordinals outside the validated prefix
scope; ordinals ≥ `W` (pending epoch — parity does not exist);
failed-block or sidecar tape files outside the durable boundary. These
are typed refusals, not recovery failures.

### 12.3. Acquiring the Sidecar Index

Locate the epoch's sidecar via the map. Then, in order:

1. Read the footer (last block). If it parses and its total matches
   the map entry: read and verify **both** header copies against the
   footer locator (including `canonical_metadata_hash`); use the
   primary if valid, else the tail; record copy health
   (both-usable / tail-lost / primary-lost).
2. If the footer is unreadable, unparseable, **or inconsistent with
   the map entry**: fall back to the primary header at block 0
   (copy-kind and map-entry block-count cross-checks apply).
3. If the primary also fails and an epoch-directory entry is
   available: locate the tail copy at
   `sidecar_total_block_count − 1 − sidecar_header_block_count` using
   the entry's counts, and verify its `canonical_metadata_hash`
   against the entry. (Appendix B item 2.)
4. Only when no header/index copy can be validated is the epoch
   metadata-unavailable — and only that epoch (§11.5).

This is the **recovery-usable rule**: *at least one valid header/index
copy + CRC-passing needed shards ⇒ the epoch is usable*. The acquired
index MUST then be pinned against the bootstrap scheme (k, m, S, block
size) and the map entry's range.

### 12.4. Erasure Taxonomy and Reconstruction

For each stripe containing a failed block, gather peers; each peer is
exactly one of:

- **Trusted shard**: read succeeded AND its CRC-64 matches the sidecar
  index (data CRC for data peers, parity CRC for parity peers) AND its
  object file is inside the durable boundary.
- **Erasure**: read failure, CRC mismatch, or outside the boundary —
  *never* a trusted shard, never poison.
- **Implicit zero**: ordinal ≥ `protected_ordinal_end_exclusive` — an
  all-zero shard, not an erasure.

Reconstruct per §5.3 from the first `k` trusted/implicit shards; more
than `m` erasures in a stripe is unrecoverable (typed, with the
counts). **Every reconstructed data block MUST be verified against its
sidecar data CRC before release**; a mismatch is an unrecoverable
result even though the matrix algebra succeeded. Bulk recoverers
SHOULD read each needed peer at most once per window, in physical
order.

## 13. Resumer Obligations

A later session appends **after the last committed tape file** — not
after the last object, not at the watermark.

1. Derive the committed prefix from the off-tape commit records
   (dropping any torn tail) and compute `W` and `T`.
2. Enforce the v1 bound: `T − W < S × k` (at most one open epoch).
   `W` MUST be epoch-aligned; `W ≤ T`; a tail object entry must end
   exactly at `T`.
3. Rebuild the open epoch by **rereading ordinals `[W, T)` from the
   committed prefix** (a boundary or short read where data is expected
   is fatal), recomputing per-block CRCs, and re-accumulating parity.
   Complete epochs encountered are re-encoded as rebuilt sidecars.
4. Write any rebuilt sidecar with the full §10.1 cycle, with one
   addition: **decode-what-you-wrote** — before writing, the encoded
   sidecar MUST round-trip through the parser and reproduce the
   planned header, index, and shard bytes exactly. The off-tape commit
   for it MUST happen only after blocks + filemark + a post-barrier
   position capture all succeed; failure abandons the boundary.
5. Position to the append point (`Σ(block_count + 1)` over the
   prefix), verify by READ POSITION, and seed the writer with: the
   prefix map, the durable boundary, `W`, the next bootstrap sequence
   (≥ the count of committed bootstraps), the live open-epoch state
   (shape- and CRC-revalidated, then re-accumulated), and **directory
   entries covering every committed-prefix sidecar one-for-one** —
   so every later bootstrap/parity_map directory still enumerates
   pre-crash epochs (the root-of-trust completeness rule).

Anything physically on tape beyond the committed prefix is superseded
by the next append and MUST NOT be trusted for recovery.

## 14. Errors

Names normative for test-vector manifests; surface syntax is not.
`NoBootstrapFound`, `NoBootstrapAtPosition`, `BootstrapParse`,
`BootstrapPayloadTooLarge`, `SidecarParse`,
`SidecarMetadataUnavailable{epoch_id}` (scoped to one epoch by
definition), `ParityMapParse`, `SchemeMismatch`,
`FilemarkMapDigestMismatch`, `FilemarkMapReconstruct`,
`OutsideValidatedMapPrefix`, `UnrecoverablePendingEpoch`,
`Unrecoverable{stripe, lost_count, limit}`, `ReedSolomon`,
`CapacityReserveExceeded`, `ObjectTooLargeForEmptyTape`,
`ResumeAppend`, `DriveCompressionEnabled`,
`DriveCompressionModeUnknown`, `Invariant`, `TapeIo`, `Journal`.
Refusals (§12.2), parse failures, and reconstruction failures MUST
remain distinguishable; I/O faults MUST remain distinct from format
violations. No code path reachable from tape bytes may panic or
allocate unboundedly: every length that drives an allocation MUST be
cross-checked against a physically measured block count first.

## 15. Security Considerations

### 15.1. No Authentication

HMAC-derived magics bind blocks to a tape UUID and role; they are
**not** authentication — the UUID is public (it is in the bootstrap),
so anyone with the tape can forge consistent structures. CRCs and
SHA-256 digests detect corruption, not tampering. The trust anchors
are external: the Layer 4 catalog/audit chain, and content-level
verification in REM-TAR-1 / the payload format. A tape that
self-validates proves self-consistency only.

### 15.2. Hostile-Input Posture

All tape bytes are untrusted. Normative bounds: every declared
count/length is validated against measured physical extent before
allocation or seek; checked arithmetic throughout; reserved fields and
declared zero-fill MUST be verified zero (misuse of reserved space is
nonconformance, and silent acceptance would foreclose 1.x extensions);
CBOR decoding enforces the §4.3 subset. Implementations SHOULD fuzz
the bootstrap parser, sidecar/parity_map parsers, and the scan walk
(§17).

### 15.3. Compression Interaction

Parity correctness assumes block-to-media identity. The recorded
`drive_compression` flag plus the dual write-time/read-time rejection
(§7.2 key 5, §10.4) exists because hardware compression silently
breaks the damage-geometry model while appearing to work.

## 16. Test Vector Requirements

Static vectors in-repo (`fixtures/rem-parity-1/`), with manifests
naming expected §14 errors for negative cases. Small geometries
(e.g. k=2, m=2, S=2, 4 KiB blocks) so full tape images are pinnable;
at least one vector at the default geometry parameters (header-level).

**Positive:** the §5.5 RS vectors plus full-stripe reconstruction for
every erasure pattern up to m; the §4.1 CRC vectors; the §6.2 digest
vector plus one multi-epoch map; a complete minimal tape image
(bootstrap + 1 object + 1 sidecar + final bootstrap) byte-pinned with
its digest chain; a final-partial-epoch image (implicit zeros); an
external parity_map image (inline overflow); a no-parity bootstrap; a
checkpoint (prefix-digest) image; a resume round-trip image (committed
prefix → reopened → appended).

**Negative (each single-fault):** bootstrap — bad magic, major=2,
header-CRC flip, payload-CRC flip, payload truncation, keys 20+21
together, compression=true with parity, oversize payload; sidecar —
each header constraint of §8.2 violated (one vector per MUST),
straddling index entry, spill CRC flip, nonzero reserved/fill, copy
disagreement, footer total vs map mismatch; parity_map — payload
SHA-256 mismatch, locator/header mismatch, directory invariant
violations (unknown flag bit, non-ascending entries, watermark
mismatch); digests — one structural-field flip per digest scalar;
recovery — m+1 erasures (typed unrecoverable), corrupt peer counted as
erasure then recovered, reconstructed-block CRC mismatch, pending-
epoch refusal, outside-prefix refusal; damage matrix — for the minimal
image, single-block damage at: object head block, sidecar primary
header, sidecar footer, sidecar footer+primary (directory-assisted
tail rescue), parity_map primary, bootstrap copy — each asserting the
specified outcome (recovered / health downgrade / one-epoch
unavailability), never whole-tape failure.

## 17. Candidate Freeze Criteria

1. The reference implementation implements this document; every
   Appendix B item closed or re-specified.
2. The §16 vectors exist and pass, including the single-block damage
   matrix and the byte-pinned minimal tape image.
3. Coverage-guided fuzzing of the bootstrap/sidecar/parity_map parsers
   and the scan walk reaches a plateau with no panics, hangs, or
   unbounded allocations.
4. A live QuadStor and a live MSL3040 round-trip pass: write with
   injected damage (chaos transport), scan catalog-less, recover, and
   verify — at two block sizes.
5. An independent exercise reconstructs the minimal tape image's map
   and recovers one damaged block using only this document and a
   generic CBOR/SHA-256/HMAC toolkit (the 30-year drill, including
   re-deriving the Cauchy matrix from §5).
6. The throughput program (review H5: table/SIMD GF and CRC) — not a
   format change, but freeze SHOULD wait for it so any fallout (e.g.
   adopting an accelerator with different internal layout) is proven
   byte-identical first via the §16 vectors.

## 18. References

**Normative:** [RFC2119]/[RFC8174] BCP 14; [RFC8949] CBOR; [RFC2104]
HMAC; [FIPS180-4] SHA-256.
**Informative:** `docs/layer3c-design-v0.7.2.md` (design rationale —
superseded for wire/conformance content by this document);
`docs/spec-v0.4.md` §9; `rem-tar-v1-candidate-specification.md`;
`docs/code-review-2026-06-10.md` (findings H3–H6 history); IBM LTO
SCSI Reference GA32-0928-08 (sense classification).

---

## Appendix A. Decisions and Superseded Material

Adjudications where code and `layer3c-design-v0.7.2.md` diverged. Rule
applied: **implemented wire layouts win** (the v0.7.2 byte tables were
never realized; no production tapes exist, but the code layout is
coherent, tested, and on every test tape); **designed semantics win**
where stronger. Each decision is recorded so refactors don't reverse
it.

**A.1. Bootstrap endianness mix is frozen.** BE header integers with
LE length/CRC fields (§7.1) — matches both code and design. Recorded
because it *looks* like an accident; normalizing it would break every
tape.

**A.2. Sidecar and parity_map layouts: code tables are normative.**
The design's §5.5/§5.6 offsets, field widths, and enum values
(u8 copy_kind 0/1, header CRC at 0xA8, index at 0xB0, `schema_version
= 2`, trailing footer-block CRCs, parity_map per-block CRCs, payload
at 0xB0) are superseded wholesale by §8–§9 (u16 copy_kind 1/2, header
CRC at 0xB0, index/payload at 0xB8, `schema_version = 1`, footer
fixed-region CRC + enforced zero fill).

**A.3. parity_map integrity = payload_sha256 + dual copy; no
per-block CRCs.** The design's per-continuation-block CRCs would
additionally enable splicing a payload from two part-damaged copies;
v1 deliberately trades that corner-case for a simpler layout — the
structure is small, dual-copied, footer-located, and hash-verified.
A future revision adding splice recovery is a layout change (new
schema_version).

**A.4. Distinct parity_map footer magic (design wins).** The
implementation reuses the header label for the footer; this document
requires `"REM\0PMAPFOOT\x01"` (§2.5), restoring role-distinct magics
as the sidecar already has. Pre-production breaking change; Appendix B
item 5.

**A.5. `drive_compression` (key 5) is part of the normative bootstrap
schema.** The design's CBOR table omitted it; the code emits and
enforces it, and §15.3 explains why it is load-bearing. Design table
superseded.

**A.6. Reader discovery is layered, not positional.** The design
removed required fractional positions; the code still probes 5% marks.
Resolution (§7.4): BOT mandatory, hints next, fractional probing
demoted to MAY (it remains a valid heuristic), bounded forward scan
normative, full filemark-walk scan a SHOULD-offer. Writer-side
content-driven placement (v0.7.2) is normative for validity rules
only.

**A.7. The recovery-usable rule is normative with a three-step
fallback ladder** (§12.3), strengthening both sides: the design's
"any one valid copy ⇒ usable" is kept, its implicit footer dependence
for tail location is resolved via the epoch directory's counts (the
directory carries `sidecar_header_block_count` and the hash precisely
so the tail is locatable and verifiable footerlessly), and a valid
footer that contradicts the map entry is treated as an invalid footer
(fall back) rather than a hard stop.

**A.8. Scan-phase classification is head-first with footer/tail
rescue and elimination fallback** (§11.3) — the post-review code
behavior, now normative, including the rule that an unreadable head
never aborts the walk. The design's overlay-time per-sidecar health
marking is dropped: health is determined at recovery time; the overlay
is structural (timing difference only; digest exclusion of health is
what matters and is kept).

**A.9. "Epoch" replaces "neighborhood".** Spec vocabulary is epoch
throughout; the code's `neighborhood*` identifiers are a naming debt,
not a semantic difference (wire format carries no such name).

**A.10. Bootstrap/parity_map timestamps are optional and currently
empty.** Keys 3/4 and 6/7 are specified optional; the reference
writer emits empty/no timestamps today. Writers SHOULD populate them;
absence is conformant.

**A.11. Resume directory completeness is a seed obligation.** The
root-of-trust completeness rule (§13 step 5) is satisfied by
caller-supplied entries validated for coverage and structure; the
format does not require re-reading prefix sidecar headers from tape at
resume time (the journal/catalog is the trusted source), but a
Verifier run SHOULD confirm entry hashes against tape.

## Appendix B. Reference Implementation Gaps (as of 2026-06-10)

The conformance backlog. Items 1–3 of the review's H-series (scan
abort, footer SPOF headline, resume directory loss) are already fixed
in the working tree and are *not* listed.

1. **Sidecar footer-inconsistent-with-map handling (§12.3 step 2)** —
   a *valid* footer whose total mismatches the map entry is a hard
   error with no primary fallback today.
2. **Directory-assisted tail rescue (§12.3 step 3)** — when footer and
   primary are both damaged, the tail copy is never attempted even
   though the epoch directory carries the counts and hash to locate
   and verify it.
3. **parity_map footer SPOF in the public parser (§9 / design
   §5.6.1)** — `parse_parity_map_tape_file` requires a valid footer;
   the primary→tail→footer fallback exists only via the scan caller's
   degradation.
4. **Scan parity_map tail probe (§11.3 item 2)** — primary-damaged
   parity_map files are rescued only via the bootstrap reference
   overlay; no tail/footer probe exists in the scan ladder.
5. **Distinct parity_map footer magic (A.4)** — code reuses the header
   label; one-line change each side plus vector updates.
6. **Candidate-size discovery error retention (§7.4)** — the
   candidate-block-size path discards the first `BootstrapParse` error
   that the known-size path preserves; unify the taxonomy.
7. **Descriptor-format filemark classification (§3.5)** — filemark
   boundary bits are decoded from fixed-format sense only; a drive
   configured for descriptor sense turns filemark boundaries into raw
   read errors (EOD already works in both formats).
8. **Full filemark-walk bootstrap scan (§7.4 step 5)** — the explicit
   opt-in last-resort scan (and the geometry-hint flag for it) does
   not exist.
9. **Static vectors and fuzz targets (§16, §17)** — none exist;
   current conformance evidence is unit/integration tests and the
   VTL-gated suite.
10. **Throughput program (§17 item 6 / review H5)** — bitwise GF and
    CRC implementations must be replaced and proven byte-identical
    via the vectors before freeze.
11. **Deterministic-CBOR decode enforcement (§4.3)** — bootstrap and
    parity_map payload decoding (ciborium defaults) accepts duplicate
    map keys and non-canonical encodings; the §4.3 MUST-reject
    requirements are not enforced. (Flagged by external review of the
    published 1.0 spec, 2026-06-11.)
