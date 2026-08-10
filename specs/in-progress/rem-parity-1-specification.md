# Rem Tape Parity (REM-PARITY) Format

## Version 1.0 — Specification

| | |
| --- | --- |
| Status | Review draft |
| Document version | 1.0 |
| Version | 1.0.0-draft.4 |
| Date | 2026-08-09 |
| License | CC-BY-4.0 |
| Concept DOI (all revisions of this document) | [10.5281/zenodo.21719156](https://doi.org/10.5281/zenodo.21719156) |
| Reference implementation (informative) | Zenodo concept DOI [10.5281/zenodo.21551570](https://doi.org/10.5281/zenodo.21551570) — software deposit, Apache-2.0 |
| Bootstrap magic | `52 45 4D 00 42 4F 4F 01` (`"REM\0BOO\x01"`, fixed bytes) |
| Erasure scheme identifier | `rs-cauchy-gf256-v1` |

## Status of This Document

**Pre-freeze draft replacement notice.** The owner has authorized a clean
replacement of the former checkpoint-bootstrap/geometric-index design. Draft
compatibility and the future post-freeze change-policy rules below do not
constrain this work. The terminal architecture has three complete final index
replicas separated by two typed extents, no intermediate index, and no singular
final bootstrap. Until this preparing copy is fully folded, the exact candidate
terminal bytes and their precedence over conflicting older bootstrap/index text
are recorded in
[`rem-parity-terminal-index-byte-draft.md`](supporting/rem-parity-terminal-index-byte-draft.md).
The publication tree remains unchanged.

**This is a review draft.** It is published for public review and is not yet
frozen. The draft.4 replacement is being implemented and independently checked;
its new vectors are not yet pinned. An independent implementation built from
the completed preparing text must eventually produce the candidate vectors.

**Comments close on 30 April 2027, and the documents freeze on 31 July 2027**,
one year after publication. On that date the finality promise below takes
effect and the change policy governs every revision after it. The three months
between the two dates exist so that changes made in response to review are
published as such, and visible, before the text is fixed. Comments and defect reports are raised as issues on the project repository.
Before reporting, check the live list of known items at
<https://archivetech.org/spec/issues> — it is current, whereas the Open Items
appendix of this document is only a snapshot taken when this revision was fixed.

On freezing, the format this document defines becomes final: no tape it
validates will ever be invalidated, no reader guarantee will be withdrawn, and
the discovery guarantee of Section 8.4 will stand for the life of
`schema_major` 1. The document itself may still be revised even then, in
exactly the three ways set out below.

No standards body has reviewed or adopted it. There is no ISO number, no RFC,
no SNIA endorsement. It was written by the same people who wrote the
implementation, so the finality above is our own undertaking and not anyone
else's approval. That is worth saying plainly, because such words usually
imply a committee somewhere, and in this case there is none.

The document's version, the bootstrap's `schema_major` and `schema_minor`
(Section 8.1), REM-OBJECT's `REMANENCE.schema_version` (a text feature gate)
and manifest `schema_version` (an integer), and REM-ENCRYPT's registries are
independent axes. None is a proxy for another, and no arithmetic relates
them. In particular, `schema_minor` is not this document's version number;
Section 8.1.1 is its registry.

**Which copy governs.** The normative text of this document is the revision
deposited under the concept DOI above. Every other copy — in the project
repository, inside a Remanence source release, on a mirror, or printed — is a
convenience copy. A copy carrying the same version string as a deposited
revision is byte-identical to it or it is defective; where they differ, the
deposit governs. A version string is never reused for different bytes, so
naming a version names one exact text no matter which copy you hold. The
reference implementation is informative: where it and this document disagree,
this document is the fixed point (Section 18, criterion 1), and the divergence
is a defect in the implementation.

**Deciding what a change is.** Every revision of this document is classified
by three questions, asked in order.

1. Does any existing tape become invalid, or does the meaning of anything
   already written change? Then it is a **new major version**: a new
   `schema_major` or new magic, and a separate document. Tapes written under
   this major keep working with readers of this major; the two formats
   coexist. An implementation MAY implement two majors at once; it then
   conforms to each major for the artifacts that major governs, and a
   reader-acceptance clause of an earlier major constrains only artifacts
   written under it.
2. Does a reader conforming only to an earlier revision lose the ability to
   identify a newer tape and refuse it cleanly? Then it is also a **major**.
   (This is why the discovery-candidate block sizes of Section 8.4 are
   frozen for the life of `schema_major` 1: a scanner that cannot rotate to
   a tape's block size does not fail with a diagnosis — it reports no
   bootstrap at all, which is indistinguishable from a blank or destroyed
   medium.)
3. Does anything an implementation must do change, or is a registry value
   assigned? Then it is a **minor revision** (1.1.0, 1.2.0, …), and it is
   permitted only if all three of the following hold: every tape written
   under an earlier revision of this major remains valid; a reader built
   from an earlier revision still reads correctly every tape that uses only
   the features defined at or before that revision; and on a tape that uses
   a feature it does not implement, that reader identifies the tape and
   refuses it with a typed error naming the unimplemented value — it never
   misreads, and never mistakes the tape for damage. A minor revision whose
   new value earlier readers must refuse MUST say so in its revision-history
   entry.

Anything else — wording, typographical errors, dead cross-references, and
clarifications that resolve an ambiguity the way conformant implementations
already behave — is an **erratum** (1.0.1, 1.0.2, …). An erratum changes no
obligation and no valid tape. One exception is named openly: a correction to
this change-policy text itself is published as an erratum and flagged as a
policy correction in the revision history, because the alternative — a
policy that cannot correct its own wording — would freeze its mistakes.

Version numbers are always three-part. A revision published for review before it is frozen carries a `-draft.N` suffix on that three-part core; it names the revision it anticipates and orders before it. The Identifiers table at the head of
this document carries both: its `Document version` row names the major.minor
line, and its `Version` row is the full revision.

For orientation, the classifications of the changes most likely to be
proposed (informative): a new optional bootstrap key — minor; a new damage
plan in the drill tooling — erratum or minor, by whether an obligation
moves; a new writer-legal block size — major, by question 2; a new
`schema_major` or magic — major, by definition.

The image-level vectors of Section 17 are conformance anchors of the
revision under which they were generated; a later revision does not make
them retrospectively non-conformant, and never re-pins them. A future
revision that needs new vectors publishes its own archive alongside this
one and cites it separately.

A tape written against this specification will therefore still be read
correctly by an implementation built from any later revision of it. A reader
does not need to know which revision wrote a tape in order to read it; the
Revision History appendix exists so that anyone can recover what a given
revision's text said, not to identify writers.

Errata are raised as issues on the project repository. Every published
revision of this document is archived with its own DOI and recorded in the
Revision History appendix.

Every conformance and freeze criterion set out in Section 18 is satisfied and
the items collected in Appendix C are closed; the criteria are formally
discharged when this document is frozen, and the review period exists so that
anyone can test that claim before it is made permanent. This document remains the
normative fixed point for the format: an implementation is validated against
it, not the reverse. The arithmetic test vectors here (CRC, Reed–Solomon,
canonical digest) are normative and can be re-derived from this text alone;
the image-level vectors of Section 17 are *pinned-at-generation*, and the
archive holding them is published with a checksum so you can confirm you
have the same bytes we do.

## Abstract

This document specifies the Rem Tape Parity (REM-PARITY) format, version 1.0:
a self-describing, parity-protected method of writing multiple archival
objects to linear tape. Objects are opaque byte strings — this format never
interprets their content — written one per filemark-delimited tape file and
protected collectively: a fixed **BOT bootstrap** identifies the tape and its
geometry;
**parity sidecar tape files** carry Reed–Solomon parity and per-block CRCs
for each parity epoch; an external final **ParityMap** carries the sidecar
directory when nonempty; and exactly three complete **terminal tape-index
replicas**, separated by two typed one-GiB extents, carry the final structural
map and Object recovery rows. Parity is computed over GF(2⁸)
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
8. [BOT Bootstrap and Terminal Index](#8-bot-bootstrap-and-terminal-index)
9. [The Parity Sidecar Tape File](#9-the-parity-sidecar-tape-file)
10. [Final ParityMap and Terminal Inventory](#10-final-paritymap-and-terminal-inventory)
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
Appendix C. [Open Items Closed Before Publication (Informative)](#appendix-c-open-items-closed-before-publication-informative)  
Appendix D. [Revision History (Informative)](#appendix-d-revision-history-informative)  
Appendix E. [Open Items (Informative)](#appendix-e-open-items-informative)  
[Author's Address](#authors-address)

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

A REM-PARITY tape is a sequence of filemark-delimited tape files. While open it
contains the BOT Bootstrap, Objects, and sidecars. Final parity closeout may
append one ParityMap immediately before the normal
finalization appends one exact five-file suffix:

```text
| bootstrap(0) | object | sidecar | ... |
| index A | gap AB | index B | gap BC | index C | EOD
```

A Writer appends objects one per tape file. As object blocks stream to tape,
the Writer accumulates Reed–Solomon parity over them in fixed-size **stripes**
grouped into **epochs**; each completed epoch's parity and per-block CRCs are
written as a **parity sidecar** tape file at the next object boundary. Epochs
span objects: the parity geometry is independent of object sizes, and an
object needs no minimum size to be protected. The BOT bootstrap records tape
identity, fixed block size, and parity scheme. There are no intermediate tape
indexes and no singular final bootstrap. Each terminal replica contains the
complete canonical structural map plus exactly one recovery row per Object. A
healthy bare-tape inventory reads BOT identity, positions from EOD to replica C,
and falls back through B then A without an Object walk. If all replicas are
invalid, recovery scans structurally from BOT; missing terminal authority is
never an empty inventory.

### 1.3. Relationship to Adjacent Components

- **Object formats above** (e.g. [REMOBJECT]) define the bytes inside object tape
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
  recorded in the BOT bootstrap. All structures in this document are sized in
  blocks of this one size. All tape I/O is in whole blocks.
- **Tape file**: a run of one or more fixed blocks delimited by exactly one
  trailing filemark. Kinds: **object**, **parity sidecar**, **bootstrap**,
  **parity map**, **tape-index replica**, **index-separation extent**.
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
- **Final prefix**: the complete committed structural prefix before terminal
  replica A; this is the identical payload scope of A, B, and C.
- **Filemark map**: the tape's structural table of contents — one entry per
  tape file (Section 7).
- **Terminal inventory**: the immutable dense structural map and matching
  Object recovery rows repeated in full by A, B, and C (Section 10).
- **Implicit zero**: a logical data position beyond a short epoch's real
  data — an all-zero shard that is never written to tape
  (Section 6.4).

### 2.4. Integer, Byte, and Text Conventions

All multi-byte integers in sidecar and terminal structures are
**little-endian**, except the BOT bootstrap fields explicitly marked
big-endian. The bootstrap header mixes endianness per field — its
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
| `BOOTSTRAP_SCHEMA_MAJOR` | 2 — replacement draft; readers MUST reject any other value (Section 8.1) |
| `BOOTSTRAP_SCHEMA_MINOR` | 3 — the reference writer's current value, not a conformance bound; see the Section 8.1.1 registry |
| `BOOTSTRAP_HEADER_LEN` | 0x38 |
| `FLAG_NO_PARITY` | bit 0 of the bootstrap flags |
| `MAX_BOOTSTRAP_SCAN_BLOCKS` | 1024 |
| Block-size discovery candidates | 256 KiB, 512 KiB, 1 MiB (Section 8.4) |
| `SIDECAR_MAGIC_LABEL` | `"REM\0PAR\x01"` (8 bytes: `52 45 4D 00 50 41 52 01`) |
| `SIDECAR_FOOTER_MAGIC_LABEL` | `"REM\0PARFOOT\x01"` (12 bytes: `52 45 4D 00 50 41 52 46 4F 4F 54 01`) |
| `PARITY_MAP_MAGIC_LABEL` | `"REM\0PMAP\x01"` (9 bytes) |
| `SIDECAR_SCHEMA_VERSION` / footer version | 2 / 2 |
| `SIDECAR_HEADER_LEN` / header CRC offset | 0xC8 / 0xC0 |
| `SIDECAR_FOOTER_LEN` / footer CRC offset | 0x88 / 0x80 |
| `PARITY_INDEX_ENTRY_LEN` | 16 |
| `DATA_CRC_ENTRY_LEN` | 8 |
| `PARITY_MAP_FORMAT_ID` / schema and footer versions | `"rem-parity-map-v1"` / 2 / 2 |
| `PARITY_MAP_HEADER_LEN` = footer len / CRC offsets | 0xC8 / 0xC0 |
| `SCHEME_ID` | `"rs-cauchy-gf256-v1"` |
| Default scheme | k = 128, m = 4, S = 512 at 256 KiB blocks (Section 6.6) |
| `SIDECAR_METADATA_HASH_DOMAIN` | `"remanence-sidecar-metadata-v1"` (29 ASCII bytes) |
| `TAPE_INDEX_REPLICA_SCHEMA_VERSION` | 1 |
| `TAPE_INDEX_REPLICA_FRAME_LEN` / CRC offset | 0x400 / 0x3F8 |
| `INDEX_SEPARATION_SCHEMA_VERSION` | 1 |
| `INDEX_SEPARATION_FRAME_LEN` / CRC offset | 0x200 / 0x1F8 |
| `TERMINAL_INDEX_REPLICA_COUNT` / `TERMINAL_INDEX_SEPARATION_COUNT` | 3 / 2 |
| `DEFAULT_INDEX_SEPARATION_BYTES` | 1,073,741,824 (1 GiB), including header and footer |
| Tape-file kind codes | Object = 0, ParitySidecar = 1, Bootstrap = 2, ParityMap = 3, TapeIndexReplica = 4, IndexSeparationExtent = 5 |
| Minimum frame sizes | bootstrap 0x40; parity map 0xC8; sidecar 0xE0; replica 0x400; separation 0x200 |

## 3. Tape Model and Address Spaces

### 3.1. Tape Files

A REM-PARITY tape is a sequence of tape files numbered densely from 0 at the
beginning of tape (BOT), each terminated by exactly one filemark, followed by
end of data (EOD):

```text
| bootstrap(0) | object | sidecar | ... |
| index A | gap AB | index B | gap BC | index C | EOD
```

Tape file 0 MUST be a bootstrap. This format owns every filemark on the
tape: an object's bytes MUST NOT depend on filemarks for internal structure,
and a Writer MUST NOT emit filemarks except as tape-file terminators. A tape
file MUST contain at least one block — an immediate filemark is structural
damage.

The **committed prefix** is the uninterrupted initial sequence of committed
tape files starting at file 0. It is empty if file 0 has not been committed;
otherwise, because numbering is dense, it contains exactly files `0..F` for
some last committed file `F`. Bytes and filemarks physically present after
`F` are not part of the committed prefix.

### 3.2. Address Spaces

- **`TapeFilePosition`** = `(tape_file_number: u64, block_within_file: u64)`.
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

An implementation MAY store the required contents of this logical commit
record in more than one durable off-tape record. If it does, it MUST designate
which records are required commit authority. Before a Resumer positions to an
append point or writes, every required authority record MUST be available,
their overlapping claims MUST agree, and their validated combination MUST
determine exactly one committed prefix, its append point, every filemark-map
entry in that prefix, and all state needed to seed the Resumer. If a required
authority record is missing, their claims conflict, or their combination is
incomplete or ambiguous, resume MUST fail as `ResumeAppend` until an
implementation-defined recovery procedure restores one unambiguous logical
commit record. A rebuildable catalog or cache not designated as commit
authority does not commit a tape file.

Finalization has a separate irreversible lifecycle:

```text
Open -> Finalizing -> Finalized
                    \
                     -> FinalizedDegraded
                    \
                     -> RecoveryRequired
```

The accepted transition to `Finalizing(BeforeReplicaA)` MUST be durable before
terminal media motion and permanently disables Object admission. Finalization
is not a pause: no failure, restart, recovery action, or degraded acceptance
may transition the tape back to `Open`. Successful component barriers advance
only through `AfterReplicaA`, `AfterSeparationAb`, `AfterReplicaB`,
`AfterSeparationBc`, and `AfterReplicaC`. Ordinary `Finalized`/sealed state
requires `AfterReplicaC` and the required host persistence order.
`Finalizing(AfterReplicaC)` is a valid resumable state: all three replicas are
barrier-proved, while the sealed checkpoint or final SQLite projection is not
yet durable. Restart MUST finish those host-only steps without repeating
terminal media motion. A matching sealed checkpoint takes precedence over a
stale companion intent left by interruption during its cleanup; a mismatch
fails closed.

A component failure or completion-unknown result enters or retains
`RecoveryRequired`. That classification is part of the fsynced companion
intent: restart MUST retain it at the same progress. A successful successor
component transition clears it. At `AfterReplicaC`, the companion MUST retain
the classification until a normalized, non-recovery sealed checkpoint is
fsynced; only then may the matching companion be retired. A failure before
that fsync therefore remains `RecoveryRequired`, while a matching sealed
checkpoint wins after it. Current capacity caps and watermarks MUST NOT be
reapplied on this host-only suffix because the reserved terminal tail is
already barrier-proved on media.
From there a Writer may reconcile and repair only missing
terminal control components at proved locations under the medium's rewrite
policy. It MUST NOT write an Object, remove the finalization fence, or append a
second terminal triple. If the next component is proved torn on WORM media and
durable progress proves exactly one or two earlier complete replicas, the
Writer MUST retain `RecoveryRequired`: each surviving replica is already a
complete final-prefix inventory, but media reconciliation is not authorization
to accept reduced redundancy. A distinct audited operator action MAY accept
that proved one- or two-replica set as `FinalizedDegraded`; this draft does not
define that action. Zero complete replicas, an unproved position, or
completion-unknown cannot be accepted; three complete replicas use ordinary
`Finalized`. A finalized-degraded tape cannot resume finalization or return to
`Open`.

Before finalization, replayable host journals are the commit authority for the
open prefix. Every barrier-proved terminal replica is a truthful complete
final-prefix inventory because `Finalizing` has already made later Objects
impossible, but no footer proves its filemark, barrier, journal fsync, or host
projection. The five-component progress record remains authoritative for those
facts.

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
   through the terminal Object-row surface (Section 8.2.1), with the
   leakage constraints stated in Section 16.4.

### 4.4. Payload Format Bindings (Informative)

**REM-OBJECT [REMOBJECT].** A REM-OBJECT meets the contract by
construction: its stored bytes are an exact positive multiple of the
object's `chunk_size` in both representations (plaintext and encrypted);
with the tape block size equal to `chunk_size`, one REM-OBJECT body block is one
tape block. Parity is computed over stored bytes — ciphertext when
the object is encrypted — so damaged encrypted objects are repaired keyless
and opening under [REMENCRYPT] is retried on the recovered bytes.

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

Sidecar, ParityMap, terminal-replica, and separation blocks carry **per-tape** magics:

```text
magic = HMAC-SHA-256(key = tape_uuid[16 bytes], message = LABEL)[0..8]
```

where HMAC is [RFC2104] with SHA-256 [FIPS180-4], `tape_uuid` is the 16-byte
tape identity from the bootstrap, `LABEL` is the role's ASCII label from
Section 2.5 (label bytes exactly as listed, including the embedded NUL and
trailing 0x01; no terminator added), and `[0..8]` takes the first 8 bytes of
the 32-byte MAC. The label bytes never appear on tape. Each block role has a
distinct label except the established ParityMap header/footer label shared by
that retained codec; its role interpretation is defined in Section 10. The
Bootstrap magic alone is a fixed byte string, because the reader does not
yet know the tape UUID when it searches for a bootstrap (Section 8.1).
Derived magics are an *identity* mechanism — these blocks belong to this
tape and role — not authentication (Section 16.1).

### 5.3. Deterministic CBOR

All CBOR frame payloads in this format are single definite-length,
integer-keyed maps in RFC 8949 deterministic encoding: shortest-form
integers and lengths, map keys sorted in ascending order of their
deterministic encodings — compared first by encoded length, then
lexicographically by encoded bytes, as [RFC8949] Section 4.2.1 specifies —
no duplicate keys, no tags, no floats, no indefinite-length items. The Section 7.3 canonical-digest preimage is not a
frame payload or a map: it applies these canonical-encoding rules to its
specified array-of-arrays structure. Each payload map MUST occupy its entire
declared payload extent (for the bootstrap, `cbor_payload_len`, Section 8.1);
bytes after the map's definite-length encoding, within the declared payload,
are a nonconformity. Decoders MUST reject duplicate keys and
non-canonical encoding, and MUST ignore unknown integer keys at every map
level — that is the format's extension mechanism: a future revision of this
document may assign new keys; it never changes the meaning of existing ones
(a change that would, requires a new schema version or magic). An assigned
key MUST NOT alter an existing field's meaning, the recovery outcome of any
tape written without it, or any other rule this document enforces; an
extension that would is a new major version, because an earlier reader
ignoring the key would recover different results with no wire signal. Allocation while decoding
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
inv(0x02) = 0x8E         inv(0x03) = 0xF4

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
| `tape_file_number` | u64, dense from 0 | all |
| `kind` | Object = 0, ParitySidecar = 1, Bootstrap = 2, ParityMap = 3, TapeIndexReplica = 4, IndexSeparationExtent = 5 | all |
| `block_count` | u64 ≠ 0 — data blocks, excluding the filemark; MUST be 1 for bootstraps | all |
| `first_parity_data_ordinal` | u64 | objects only |
| `protected_ordinal_start` / `protected_ordinal_end_exclusive` | u64, half-open range | sidecars only |
| `epoch_id` | u64 | sidecars only |

### 7.2. Validity

Tape file numbers are dense from 0. Object first-ordinals are dense and
contiguous from 0 in tape order (Section 3.2). Kind-specific fields are
exclusive to their kinds; an entry carrying a field outside its kind is
invalid. A terminal-replica payload describes only the prefix before A, so
kinds 4 and 5 inside that payload are invalid even though scanners recognize
those kinds in the measured physical tail. Derived scalars:

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

**Exclusions — the non-circularity rule.** The canonical map digest covers no
physical position hints, no content hashes, no control-file payload bytes, and
no replica-health state. Terminal replica envelopes separately bind the
complete planned five-file layout while their canonical payload remains the
prefix before A. Discovering damage therefore changes health evidence, not the
canonical inventory.

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

In each terminal replica envelope, the digest travels
with its scope:

| Field | Meaning |
| --- | --- |
| `sha256` | the canonical digest |
| `tape_file_count` | the prefix length (number of leading tape files) the digest covers |
| `map_total_data_ordinals` | `T` over that prefix |
| `highest_protected_ordinal` | `W` over that prefix |
| `is_final_map` | true for a terminal final-prefix inventory |

Validation MUST recompute the digest over exactly the leading
`tape_file_count` entries and cross-check all three scalars. A terminal digest
yields a **Complete final-prefix** map. The five terminal files are checked
against the separate planned layout and observed placement; they are not
recursively embedded in the payload map. Recovery MUST be fenced to the
validated scope (Section 13.2).

## 8. BOT Bootstrap and Terminal Index

The BOT bootstrap is the tape's UUID-independent entry point: a single block at
tape file 0, findable by magic, that records the tape's identity, block size,
and parity scheme. It is the only
structure in this format with a fixed (non-derived) magic, because the
reader does not yet know the tape UUID when searching for it.

The replacement profile writes no later Bootstrap copy. Every draft.4
Bootstrap is Object-count independent: its payload contains no Object recovery
rows, including on a no-parity tape. Host checkpoint operations do not emit a
Bootstrap. Keys 2, 20, 21, and 30
of the earlier draft are not terminal inventory authority and a replacement
Writer MUST NOT use them to publish checkpoint or final indexes. Final
structural and Object rows live only in the streamed terminal replicas defined
in Sections 8.2.1 and 8.3.

### 8.1. Fixed Frame (one block, exactly)

A bootstrap tape file is exactly one block:

| Offset | Len | Field | Type | Constraint |
| --- | ---: | --- | --- | --- |
| 0x00 | 8 | magic | fixed bytes | `BOOTSTRAP_MAGIC` |
| 0x08 | 2 | schema_major | **u16 BE** | MUST be 2; readers reject ≠ 2 |
| 0x0A | 2 | schema_minor | **u16 BE** | registry in Section 8.1.1; readers accept any value; payload rules MAY gate on it |
| 0x0C | 4 | flags | **u32 BE** | bit 0 = no-parity; writers MUST zero all other bits; readers MUST ignore bits they do not recognise. A future flag may therefore carry only semantics an earlier reader can safely ignore; anything stronger requires a new `schema_major` |
| 0x10 | 16 | tape_uuid | raw bytes | the tape's identity (16 opaque bytes; RECOMMENDED a version-4 UUID [RFC9562], unique per tape); the HMAC key of Section 5.2 |
| 0x20 | 4 | block_size_bytes | **u32 BE** | MUST equal the size of the block it was read with |
| 0x24 | 8 | sequence | **u64 BE** | exactly 0 for the sole BOT bootstrap |
| 0x2C | 4 | cbor_payload_len | **u32 LE** | payload byte length |
| 0x30 | 8 | crc64_header | **u64 LE** | CRC-64/XZ over bytes 0x00..0x30 |
| 0x38 | var | CBOR payload | Section 8.2 | |
| +len | 8 | crc64_payload | **u64 LE** | CRC-64/XZ over the payload bytes |
| … | | zero fill to block end | | MUST be written zero; not an acceptance rule (see below) |

The endianness mix — big-endian header integers, little-endian length and
CRCs — is fixed by this replacement draft; implementations MUST NOT
"normalize" it (Appendix B.5). The minimum viable block size is 0x40 (header
plus the payload CRC of an empty payload). Parse order: block length ≥ 0x40 → magic →
header CRC → schema_major → payload bounds (checked against the block) →
payload CRC → CBOR.

Writers MUST zero the trailing fill. Verifiers MUST verify it and report a
nonzero fill as a nonconformity, but bootstrap *acceptance* — during
discovery (Section 8.4) and classification (Section 12.3) — MUST NOT depend
on the fill, so a damaged fill byte cannot cost the tape its entry point.
This is the one deliberate exception to the Section 16.2 verify-zero rule.

#### 8.1.1. Schema Minor Registry

`schema_minor` names generations of the bootstrap wire format. It is not a
revision counter and not this document's version number: no arithmetic
relates it to anything, most revisions of this document assign no new value,
and a value is assigned only when a wire-visible change warrants signaling,
by the document revision that defines the change.

| `schema_minor` | Status | Defined by | Wire meaning |
| ---: | --- | --- | --- |
| 0–1 | historic | pre-publication drafts | development-era values; absent from the published vectors |
| 2 | historic major-1 | REM-PARITY 1.0 | publication-baseline bootstrap payload generation |
| 3 | **current major-2 writer value** | REM-PARITY draft.4 | no terminal-index semantics; Object rows are carried by terminal replicas |
| ≥ 4 | unassigned | a future revision of this document | a value here indicates a tape written under a later revision; retrieve the current revision via this document's concept DOI |

Readers accept any value in the fixed frame — that is deliberate, and unlike
`format_version` in REM-ENCRYPT, whose unassigned values are hard errors.
Individual future payload rules MAY be gated on the value. A replacement Writer
SHOULD emit the current value. The unchanged publication archive contains
major-1 images at minor values 2 and 3; those bytes are not replacement-draft
vectors and major-2 readers reject their narrow bootstrap authority.
An unassigned value is not an error here: Section 5.3's ignore-unknown rule
means a reader reads through newer bootstrap revisions, satisfying the
change policy's second condition outright. The refuse-with-typed-error
condition governs formats that gate reading on a registry, which the
bootstrap does not.

To identify what defines a tape in hand: read `schema_major` and
`schema_minor` from any bootstrap (Section 8.1) — each assigned value's
defining revision is named in this registry; read the terminal Object rows for the
identities and geometry of what is stored (Section 8.2.1). Every revision of
this document is retrievable through its concept DOI. Which revision *wrote*
the tape is not recorded on the wire and is not needed for reading; treat
`schema_minor` as provenance only where this registry gives it a wire
meaning.

### 8.2. CBOR Payload

A single integer-keyed map (Section 5.3):

| Key | Type | Presence | Meaning |
| ---: | --- | --- | --- |
| 1 | map | REQUIRED unless no-parity | scheme record: `{1: tstr scheme_id, 2: uint k, 3: uint m, 4: uint S}` |
| 2 | map | REQUIRED unless no-parity | BOT-only digest record: `{1: bytes .size 32 sha256, 2: uint tape_file_count=1, 3: uint map_total_data_ordinals=0, 4: uint highest_protected_ordinal=0, 5: bool is_final_map=false}` |
| 3 | tstr, ≤ 128 bytes | REQUIRED (a Writer MUST write it); readers MUST tolerate absence | writing-implementation identity; printable US-ASCII only |
| 4 | tstr, ≤ 64 bytes | OPTIONAL | [RFC3339] write timestamp |
| 5 | bool | REQUIRED (a Writer MUST write it); readers MUST treat absence as false | `drive_compression` — effective hardware compression at session open. `true` on a parity bootstrap MUST be rejected (Sections 8.4, 11.4) |
| 20 | map | MUST be absent | legacy inline sidecar directory; terminal close writes an external ParityMap when required |
| 21 | map | MUST be absent | legacy Bootstrap locator; the external ParityMap is covered as a structural row in every terminal replica |
| 30 | array of maps | MUST be absent | legacy cumulative bootstrap Object rows; replacement rows are fixed slots in each terminal replica (Section 8.2.1) |

Any of keys 20, 21, or 30 in a replacement BOT bootstrap is a parse error. A
**no-parity bootstrap** (flag bit 0 set) marks a tape written without parity
protection; it MAY omit the scheme record (key 1) and the digest record
(key 2), and readers MUST NOT require those records on it. For a parity bootstrap the
scheme record's `scheme_id` MUST be `rs-cauchy-gf256-v1` and `(k, m, S)`
MUST satisfy Section 6.6 validity. Unknown keys are ignored at every level
(Section 5.3). Writers SHOULD populate keys 3 and 4; absence is conformant.

No Reader decision defined by this document depends on key 3 or key 4. They
exist for a different reader: the person holding a cartridge that does not
decode as this document says it should. A tape is produced by an
implementation, not by a specification, and implementations have defects. When
the bytes and this document disagree, the only thing that resolves the
disagreement is knowing which software wrote them, so that its behaviour at
that version can be established. Key 3 is that record. It is required for the
same reason a conformance claim is not a substitute for it: the claim states
an intention, and the tape is the result.

A Writer MUST emit key 3, as at most 128 bytes drawn from printable US-ASCII
(`0x20`–`0x7E`), identifying the software that wrote the bootstrap. It SHOULD
take the form `<implementation>/<version>`, optionally followed by a space and
a parenthesised build identifier — for example
`remanence/1.0.0 (v1.0.0-12-g874b111)`. The implementation part names the
software, not the format: two conformant implementations of this document will
not agree on it, and are not expected to. A Writer SHOULD emit key 4, as at
most 64 bytes forming a valid [RFC3339] `date-time`.

A Reader MUST tolerate the absence of either key, and MUST treat a value
violating either rule exactly as it treats that key's absence, for every
purpose. It MUST NOT refuse the bootstrap, the tape file, or the tape on
account of either. A Reader that renders either value MUST escape it, so that
no part of it can be interpreted as a control or formatting instruction by
whatever receives the output.

The reader obligations above are what every conformant Reader already does
with an absent key, so requiring key 3 of Writers costs earlier tapes nothing:
a tape written without it, or by an implementation that predates this rule,
remains valid and fully readable.

#### 8.2.1. Terminal REM-OBJECT Recovery Rows

After all 64-byte structural slots, every terminal replica carries one
256-byte fixed slot for each Object row, in strictly increasing
`tape_file_number` order. Each slot is `encoded_len:u16 LE`, deterministic CBOR
of the following integer-keyed map, then zero padding:

| Row key | Type | Presence | Meaning |
| ---: | --- | --- | --- |
| 1 | uint | REQUIRED | filemark-delimited object `tape_file_number` |
| 2 | tstr | REQUIRED | representation marker: `"plaintext"` or `"encrypted"` |
| 3 | uint | REQUIRED | stored block count for the object tape file |
| 4 | bytes, 1–64 | REQUIRED | REM-OBJECT `object_id` — the identity the archive answers "where is object X" with — carried verbatim as its 1–64 non-NUL bytes (any REM-ENCRYPT envelope NUL padding from [REMENCRYPT] §5.2 stripped). This matches [REMOBJECT] §4.5.1 exactly: opaque UTF-8, 1–64 bytes, with no conversion. |
| 10 | uint | plaintext only | `manifest_first_chunk_lba` |
| 11 | uint | plaintext only | `manifest_size_bytes` |
| 12 | uint | plaintext only | `manifest_chunk_count` |
| 13 | bytes .size 32 | plaintext only | `manifest_sha256` |
| 21 | uint | encrypted only | REM-ENCRYPT header `metadata_frame_len`; bounds `[17, 16 MiB]` |
| 22 | array of bytes .size 16 | encrypted only | REM-ENCRYPT key-frame `recipient_epoch_id` values; 1 through 8 distinct nonzero ids |
| 23 | uint | encrypted only | REM-ENCRYPT header `key_frame_len`; bounds `[1191, 16384]` |

Row keys are integer field names, unrelated to value sizes or positions.
They are grouped — 1–4 identity, 10–13 plaintext-only, 21–23 encrypted-only
— and the gaps between groups are unassigned space reserved so a future
revision can place a new field beside its relatives (Section 5.3 governs
assignment; unknown keys are ignored).

Key 10 (`manifest_first_chunk_lba`) is the zero-based block index, *within
the object's tape file*, of the first block of the manifest entry's payload —
a REM-OBJECT inner `BodyLba`, not a Section 3.2 Logical LBA; one REM-OBJECT body block is
one tape block (Section 4.4). Key 11 is the manifest's payload byte length,
key 12 its block count, and key 13 the SHA-256 of its CBOR bytes. Key 21 is the
REM-ENCRYPT header's `metadata_frame_len`; key 22 records the
recipient epoch ids present in its key frame; key 23 is the header's
`key_frame_len`. The semantics of these three keys, and the key 21 and key 23
bounds, are defined by [REMENCRYPT], which is a normative reference for
implementations of terminal Object recovery rows; the requirement that key 22's `recipient_epoch_id`
values be distinct and nonzero is imposed by this document, for the benefit of
a catalog-less scan that must tell recipient slots apart.

Plaintext rows MUST carry keys 10–13 and MUST NOT carry keys 21–23.
Encrypted rows MUST carry keys 21–23 and MUST NOT carry keys 10–13. For
plaintext rows, `manifest_chunk_count` and `manifest_size_bytes` MUST be
positive, the manifest chunk range MUST fit within `stored_block_count`, and
the manifest byte length MUST fit within `manifest_chunk_count ×
block_size_bytes`. For every row, `stored_block_count` MUST be positive and
MUST match the structural filemark-map row for key 1.

The row set is the complete final-prefix Object inventory. Its count MUST equal
the number of kind-0 structural slots, and the ordered `(tape_file_number,
stored_block_count)` pairs MUST be a bijection with those slots. A resumed open
Writer preserves the replayable row authority off tape and appends new rows;
finalization streams the complete set into A, B, and C. No whole-index
allocation is required.

There is no one-block Object-row ceiling. Admission instead includes the
checked terminal payload `64*S + 256*R`, three rounded replica records, both
separation extents, parity closeout, filemark charges, and safety allowance.
Overflow or insufficient capacity refuses before Object motion.

### 8.3. Placement (Writer)

Exactly one bootstrap is mandatory at BOT, with sequence 0. No intermediate or
final bootstrap is permitted. After final parity closeout, a normal Writer
emits exactly:

```text
TapeIndexReplica A       + filemark + barrier
IndexSeparationExtent AB + filemark + barrier
TapeIndexReplica B       + filemark + barrier
IndexSeparationExtent BC + filemark + barrier
TapeIndexReplica C       + filemark + barrier
EOD
```

The candidate byte layout, digest domains, and field tables are defined in the
[terminal byte draft](supporting/rem-parity-terminal-index-byte-draft.md) while draft.4
remains experimental. Each replica uses one full header record, one or more
payload records, and one full local footer record. Each default gap includes
its header and footer within `ceil(1 GiB/block_size)` records. The shared
planned layout is computed before A; local observations in a footer MUST equal
that plan, but planned future components never prove their existence.

### 8.4. Discovery (Reader)

A Scanner with no off-tape state first reads the sole Bootstrap at BOT to
establish tape identity, block size, and parity geometry. It then positions to
EOD and discovers terminal index replicas in newest-first order:

1. locate and validate replica C's local footer, body, and header;
2. if C is absent or invalid, locate and validate replica B;
3. if B is absent or invalid, locate and validate replica A;
4. if no replica validates, perform the BOT structural recovery walk in
   Section 8.4.1.

Footer-local observed positions MUST agree with the footer's planned shared
layout before that replica is eligible. A planned position or digest for a
later component does not prove that the component was written. Filemarks and
EOD are structural evidence and are not represented as payload bytes.

When the block size is unknown, the discovery candidates (256 KiB, 512 KiB,
1 MiB) MUST each be applied as a real drive reconfiguration before reading;
a parsed bootstrap is accepted only if its `block_size_bytes` equals the
configured read size.

A conformant Writer for production media MUST use one of the discovery-candidate
block sizes. This closes the writer-legal set over the discovery set: every
conformant tape is discoverable from the media alone, with no out-of-band
hint. A
Scanner MUST nevertheless accept an operator-supplied block-size hint and
apply it as a configured read size — the hint path serves damaged-media
recovery and nonconformant tapes, not Writer freedom.

Replacement-draft terminal vectors use the same 256 KiB, 512 KiB, and 1 MiB
record sizes as conformant media. The frozen publication archive contains
historical 4096-byte major-1 images; those bytes are verified by the isolated
publication tools and are not terminal-tail vectors. An operator may still
supply another size to the separate BOT structural walk for damaged or
nonconformant media, but that hint cannot make a terminal replica or separation
extent at that size eligible.

The terminal byte draft defines the exact local eligibility checks. A magic or
CRC miss invalidates that candidate and triggers the next older replica. A
medium error invalidates the affected candidate, while a non-medium transport
error aborts discovery. `drive_compression = true` on the BOT Bootstrap still
rejects the tape (Sections 11.4, 16.3).

#### 8.4.1. All-Replicas-Invalid BOT Walk

When A, B, and C are all absent or invalid, the Scanner MUST offer a full
structural walk from BOT. The BOT Bootstrap supplies tape identity and geometry
when readable. If it is unreadable, an operator-supplied block-size hint and
expected tape-identity UUID are required. The identity hint is required because
terminal, ParitySidecar, and ParityMap frame magics are derived from the tape
UUID; geometry alone cannot derive, validate, or safely classify those frames.
The walk reconstructs tape-file boundaries, validates recognisable
ParitySidecar and control structure, and measures complete Object candidates
by elimination. It reports each candidate's identity as unknown unless a
separate REM-OBJECT recovery pass succeeds. It does not invent a terminal
replica and it MUST report that terminal authority was not recovered.

- **Operator acknowledgement.** The walk traverses the entire medium and can
  take hours. A Scanner MUST report the fallback before starting and MUST
  remain abortable between tape files.
- **Identity and geometry hints.** A Scanner MUST accept hints supplied out of
  band (catalog, journal, medium auxiliary memory, operator): expected tape
  UUID, block size, expected tape-file count, expected capacity. Expected tape
  UUID and block size are mandatory when the BOT Bootstrap is unreadable;
  tape-file count and capacity remain optional. A block-size hint makes the
  size known and is applied as a configured read size under the Section 8.4
  hint path, suppressing candidate rotation. A Scanner MAY use the count and
  capacity hints to compute progress estimates. Hints MUST NOT cause any tape
  file to be skipped.
- **Progress.** The Scanner MUST report progress at least once per tape file
  crossed: the current tape-file ordinal, the current position as a logical
  block address (with partition where applicable), structural candidates
  found so far, and elapsed time. A walk that emits no
  progress is nonconformant even if it terminates correctly.
- **Abort.** The walk MUST be abortable between tape files. On abort the
  Scanner MUST report the extent walked (the last tape-file ordinal crossed),
  the candidates found, and the drive's best-known position — or state
  explicitly that position is indeterminate when the drive cannot report one.
  An operator who aborts is planning a next step and needs to know where the
  head is.
- **Positioning-failure bound.** During this BOT structural walk, inter-file
  positioning commands (SPACE and any LOCATE issued between tape files) are
  governed by this bullet; a read failure remains a read failure and does not
  consume this separate positioning-failure budget. After
  `WALK_MAX_CONSECUTIVE_POSITIONING_FAILURES` (8) consecutive positioning
  failures the walk MUST stop and report rather than continue commanding
  motion against a medium or drive that is refusing it.
- **Termination.** The walk ends at EOD. Encountering EOM first is reported
  as truncation and feeds the Section 12.6 tail taxonomy unchanged.

### 8.5. Authoritative Selection

Among locally valid terminal replicas, selection order is C, then B, then A.
Every pair of surviving replicas MUST agree on their edition digest, layout
digest, fixed pre-A scope, counts, and ordered payload records. Disagreement
is `TerminalIndexReplicaConflict`; a Scanner MUST NOT choose one side of a
conflict merely because it is newer in the suffix. If no replica validates,
selection yields the explicit BOT structural recovery path of Section 8.4.1.

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
size for sidecars is 0xE0 — block 0 must hold the 0xC8-byte header, at least
one 16-byte parity index entry, and the trailing 8-byte CRC.

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
| 0x50 | 8 | parity_block_count u64 | MUST = S × m |
| 0x58 | 8 | data_crc_count u64 | MUST = real_data_shard_count |
| 0x60 | 8 | sidecar_header_block_count u64 (H) | MUST equal the recomputed layout (Section 9.4) |
| 0x68 | 8 | inline_index_entry_bytes u64 | MUST equal the recomputed layout (Section 9.4) |
| 0x70 | 8 | sidecar_total_block_count u64 | = 2H + P + 1 |
| 0x78 | 8 | primary_header_start_block u64 | MUST = 0 |
| 0x80 | 8 | tail_header_start_block u64 | MUST = H + P |
| 0x88 | 8 | footer_block_index u64 | MUST = 2H + P |
| 0x90 | 2 | copy_kind u16 | 1 = primary, 2 = tail |
| 0x92 | 2 | reserved | MUST be 0 |
| 0x94 | 4 | copy_generation u32 | MUST be 0 while sidecar `schema_version` = 2 |
| 0x98 | 32 | canonical_metadata_hash | Section 9.5 |
| 0xB8 | 8 | reserved u64 | MUST be 0 |
| 0xC0 | 8 | header_crc64 | CRC-64/XZ over bytes 0x00..0xC0 |
| 0xC8 | var | inline index entries | Section 9.3 |
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
(offset 0xC8) and spills into blocks 1..H−1; **an entry never straddles a
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
offset = 0xC8                            (block 0: entries start after the header)
blocks = 1
inline = unset

for each entry e in stream order (parity entries, then data-CRC entries):
    len = 16 if e is a parity entry else 8
    if offset + len > limit:
        if offset == (0xC8 if blocks == 1 else 0):
            reject: block_size cannot hold a len-byte index entry
        if blocks == 1 and inline is unset:  inline = offset − 0xC8
        blocks += 1
        offset  = 0                      (spill blocks: entries start at 0)
    offset += len

if inline is unset:  inline = offset − 0xC8
H = blocks
```

A block size too small to hold a single 16-byte entry — in block 0 after the
header, or in a spill block — is invalid for sidecars (minimum 0xE0,
Section 2.5). Because every sidecar carries at least one parity entry
(Section 9.2 requires `S` and `m` to be non-zero), sizes 0xD0 through 0xDF
satisfy the header-plus-CRC floor but still cannot pack the index, and are
rejected by the guard above. A worked computation at the default geometry is in
Appendix A.2.

### 9.5. The Canonical Metadata Hash

`canonical_metadata_hash` is SHA-256 over, in order:

1. the domain string `"remanence-sidecar-metadata-v1"` (29 ASCII bytes, no
   terminator);
2. the header's exact wire bytes 0x00 through 0x8F inclusive — magic through
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
| 0x08 | 2 | footer_version u16 = 2 |
| 0x0A | 2 + 4 | reserved, MUST be 0 |
| 0x10 | 16 | tape_uuid |
| 0x20 | 8 | epoch_id u64 |
| 0x28 | 8 + 8 | protected_ordinal_start / protected_ordinal_end_exclusive |
| 0x38 | 8 | H u64 (`sidecar_header_block_count`) |
| 0x40 | 8 | P u64 (`parity_shard_block_count`) |
| 0x48 | 8 | sidecar_total_block_count u64 |
| 0x50 | 8 | primary_header_start_block u64 = 0 |
| 0x58 | 8 | tail_header_start_block u64 = H + P |
| 0x60 | 32 | canonical_metadata_hash |
| 0x80 | 8 | footer_crc64 — CRC-64/XZ over bytes 0x00..0x80 |
| 0x88… | | zero fill, MUST be zero |

The footer is a *locator*: it holds everything needed to find and check
either header copy without reading the other, and it sits at the end of the
tape file where it is found by reading the file's last block (Section 12.3
item 4, Section 13.3).

## 10. Final ParityMap and Terminal Inventory

When final parity closeout has a nonempty sidecar directory, the Writer emits
exactly one external `rem-parity-map-v1` ParityMap before replica A. It retains
the established two independently readable rounded header-plus-payload copies
and one locator footer, followed by one filemark. Its payload carries the final
SidecarEpochDirectory, canonical prefix digest, `u64` sequence and structural
scope fields, and bounded diagnostic strings. CRC, payload SHA-256, copy/footer agreement,
deterministic CBOR, and directory invariants all remain mandatory.

The primary header, tail header, and footer use this common little-endian fixed
layout. Headers set `copy_kind` to 1 or 2; the footer reserves that field as
zero. The header/footer version is 2. The payload immediately follows byte
`0xC8` in each header copy.

| Offset | Len | Field |
| --- | ---: | --- |
| 0x00 | 8 | HMAC-derived ParityMap magic |
| 0x08 | 2 | schema/footer version u16 = 2 |
| 0x0A | 2 | copy_kind u16 (header 1/2; footer 0) |
| 0x0C | 4 | reserved, MUST be zero |
| 0x10 | 16 | tape_uuid |
| 0x20 | 8 | sequence u64 |
| 0x28 | 4 | block_size u32 |
| 0x2C | 4 | reserved alignment field, MUST be zero |
| 0x30 | 8 | payload_len u64 |
| 0x38 | 32 | payload_sha256 |
| 0x58 | 32 | canonical_map_digest |
| 0x78 | 8 | directory_scope_tape_file_count u64 |
| 0x80 | 8 | directory_scope_total_data_ordinals u64 |
| 0x88 | 8 | directory_scope_highest_protected_ordinal u64 |
| 0x90 | 1 | is_final_directory, exactly 0 or 1 |
| 0x91 | 7 | reserved, MUST be zero |
| 0x98 | 8 | copy_block_count u64 |
| 0xA0 | 8 | parity_map_total_block_count u64 |
| 0xA8 | 8 | primary_copy_start_block u64 = 0 |
| 0xB0 | 8 | tail_copy_start_block u64 |
| 0xB8 | 8 | footer_block_index u64 |
| 0xC0 | 8 | CRC-64/XZ over bytes 0x00..0xC0 |
| 0xC8… | | payload in headers; zero fill in footer |

The ParityMap is parity-closeout metadata and one structural row in the fixed
pre-A prefix; it is not terminal inventory authority. A draft.4 Writer MUST NOT
emit intermediate ParityMaps, checkpoint indexes, or a singular final index.
The complete authoritative inventory is the fixed-slot payload repeated in
full by replicas A, B, and C.

### 10.1. Structural rows

The payload begins with exactly `S` 64-byte slots in dense tape-file order.
Each slot carries deterministic CBOR for one structural entry:

```text
tape_file_number:u64
kind:u64
block_count:u64
first_data_ordinal:?u64
protected_start:?u64
protected_end:?u64
epoch_id:?u64
```

Kinds 0, 1, 2, and 3 mean Object, ParitySidecar, the sole BOT Bootstrap, and
the optional final ParityMap.
Kinds 4 and 5 identify terminal replicas and gaps but MUST NOT occur in this
payload because its scope ends immediately before A. Every structural and
ordinal-range invariant is validated while the rows stream; the implementation
does not need a tape-wide allocation.

### 10.2. Object recovery rows

The structural slots are followed by exactly `R` 256-byte slots, one for each
kind-0 structural row and in the same tape-file order. The representation fields
and bounds are those in Section 8.2.1. Tape-file numbers, stored-block counts,
manifest positions, manifest sizes, and manifest counts are `u64`; the
REM-ENCRYPT key-frame length retains its bounded `u32` format constraint.

### 10.3. Framing and digests

The exact slot prefix, header/footer fields, digest domains, planned
five-component tuples, and decoder order are specified by the
[terminal byte draft](supporting/rem-parity-terminal-index-byte-draft.md). The payload
digest covers every complete fixed slot including zero padding. The canonical
map digest covers the deterministic structural projection. The edition digest
binds the immutable snapshot facts shared by A/B/C; the layout digest binds
only the planned A/gap/B/gap/C layout. Footer-local observed fields are not
included in the shared layout digest.
## 11. Writer Obligations

### 11.1. Commit Discipline (per tape file)

Every tape file — object, sidecar, Bootstrap, ParityMap, terminal replica, or separation
extent — goes through one cycle:

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
operation on the session. One exception: a write failure — of a block or of
a filemark — whose completion state is *known* (the failed write consumed no
position) MAY leave the writer usable; a completion-unknown failure MUST
poison. The
watermark `W` advances only after a sidecar's boundary commit.

### 11.2. Epochs and Sidecars

Parity accumulates incrementally per data block (Section 6.3): the writer
holds `S × m` block-sized accumulators and one CRC per pending data block —
bounded memory regardless of object sizes. At `S × k` data blocks the epoch
closes into a **pending** sidecar held in memory or spool; no tape I/O
occurs mid-object. Pending sidecars are emitted as tape files when the
current object closes. A barrier may close a non-empty short epoch and emit
its sidecar without `FINAL_PARTIAL_EPOCH`. Finalization performs the same
funnel with a terminal reason: its short epoch, if any, carries
`FINAL_PARTIAL_EPOCH`, after which the Writer emits the final ParityMap when
required and then the exact terminal triple.
Implicit-zero positions never cause padding blocks to be written (Section 6.4).

After each object's close-and-emit bundle, unprotected ordinals MUST number
fewer than `S × k` — the version-1 bounded-restart rule: at most one open
epoch ever needs rebuilding (Section 14 step 2).

### 11.3. Finalization

Finalization is permitted only between objects; a mid-object request MUST be
refused. It closes the partial epoch, emits all remaining sidecars, writes one
final ParityMap when the resulting sidecar directory is nonempty, fixes one
immutable snapshot including that structural row, and writes exactly replica A,
separation AB, replica B,
separation BC, and replica C, each followed by a filemark and persistence
barrier. A finalized tape accepts no further appends.

The first durable finalization transition permanently disables Object
admission. Failure enters `RecoveryRequired`; recovery may emit only missing
terminal control components at positions proved by the durable progress state.
It cannot reopen the tape or write a second terminal triple (Section 3.4).

### 11.4. Session Preconditions

Drive hardware compression MUST be verified off — set, then read back —
before any parity write; the effective value is recorded as bootstrap key 5,
and a parity bootstrap recording `true` MUST be rejected by readers and
writers alike (Section 16.3). Capacity admission MUST preserve parity closeout,
three rounded replica files, two complete separation extents, five filemark
charges, and the configured safety allowance before admitting an Object. A
writer MUST treat hard end-of-medium mid-file as a commit failure (Section
11.1), never as a signal to truncate an object.

## 12. Scanner Obligations

### 12.1. Inputs and Authority

An off-tape catalog is a cache. The mounted tape's BOT Bootstrap establishes
identity, and a finalized tape's selected terminal replica supplies the
authoritative inventory. Before finalization, the durable host journals govern
the replayable open prefix and append position. If terminal selection yields no
valid replica, the Scanner returns the explicit BOT structural-recovery outcome
and walks from LBA 0.

### 12.2. The Walk

Per tape file: read the head block; measure the file's length by filemark
spacing (space to the next filemark; the file's block count is the position
delta minus one); a zero-block file or a missing trailing filemark is
structural damage; EOD at a file start ends the walk.

### 12.3. The Classification Ladder (in order)

1. **Bootstrap**: the fixed magic matches, the full frame parses, the
   frame's `block_size_bytes` equals the read size, the payload's
   `tape_uuid` equals the tape identity of Section 12.1, and the file measures
   exactly 1 block.
2. **TapeIndexReplica**: matching terminal-replica header magic commits the
   tape file to this control type. The header and measured block count are
   checked against the encoded component plan. A malformed frame or count
   mismatch is reported as damaged terminal control; it MUST NOT fall through
   to Object. When the head is unreadable, a matching, fully parsed terminal
   footer may establish the same type after its measured count is checked.
3. **IndexSeparationExtent**: matching separation-header magic commits the
   tape file to this control type under the same malformed-control and measured
   count rules. A matching, fully parsed footer may establish the type when the
   head is unreadable.
4. **ParityMap**: a complete copy/header and payload validate, and the measured
   block count agrees with its locator footer. It is classified as pre-tail
   parity-closeout metadata, not selected as terminal inventory authority.
5. **Sidecar (primary)**: the primary header parses; the measured block
   count MUST equal the header's `sidecar_total_block_count` (mismatch is a
   hard error).
6. **Sidecar (footer/tail probe)**: read the file's last block; if it
   parses as a sidecar footer, the footer's total MUST equal the measured
   block count; then verify the tail header copy against the footer,
   field for field. Classification MAY fall back to footer fields alone if
   the tail copy is unreadable.
7. **Object, by elimination** — never by reading object content.

In items 2 through 6, a count-mismatch “hard error” is scoped to that rung and
that tape file's classification: the Scanner reports the failed
classification and continues the walk with the next tape file. It MUST NOT
abort the whole walk for that mismatch.

**Unreadable head block:** the Scanner MUST NOT abort. It MUST measure the
file by filemark spacing, run the footer/tail sidecar probe, and otherwise
classify the file as an object candidate. This is the no-circular-failure
rule in action: the block needing recovery may be the very block that would
have classified the file.

### 12.4. Terminal Replica Validation

The Scanner first performs bounded terminal discovery from EOD (Section 8.4).
A candidate replica is locally eligible only when its header, streamed fixed-slot
payload, footer, local observations, planned layout, trailing filemark, CRCs,
and all digests validate. Selection prefers C, then B, then A.

Every surviving envelope MUST agree with the selected edition's identity,
scope, counts, payload digest, canonical-map digest, diagnostic fields, and
layout digest. Any disagreement is `TerminalIndexReplicaConflict`; it is not
resolved by ordinal preference. A missing or corrupt member is reported as
degraded evidence without invalidating an agreeing survivor.

On the ordinary healthy path, where the envelope evidence identifies one
agreeing edition without conflict resolution, the Scanner reads each attempted
body at most once and emits a bounded transactional stream. Every attempt
starts with a unique `attempt_id`; its map
and Object rows carry that identifier and remain provisional until the terminal
summary selects the attempt. If validation fails after any rows, the Scanner
emits an explicit rejection for that attempt before trying the next member.
A Consumer MUST commit only the attempt named by the terminal summary and MUST
discard rejected or unselected attempts. This permits ordinary C-to-B-to-A
fallback with downstream backpressure and without a whole-index buffer or a
second tape-body read. When independently valid envelopes conflict, resolving
which editions have payload-valid survivors may require a bounded replay of
candidate bodies, and the selected transactional attempt may then replay the
winner for its consumer. That exceptional conflict path remains streaming and
bounded; it does not weaken the fail-closed cross-survivor comparison reflected
in the terminal summary.

If A, B, and C all fail, the Scanner returns an explicit
`BotStructuralRecoveryRequired` outcome and performs the Section 8.4.1 walk;
BOT Object classifications are likewise streamed before their terminal
summary. It MUST NOT convert missing terminal authority into an empty inventory
or infer the existence of a planned future component from A or B.
### 12.5. Epoch Isolation

Damage confined to one sidecar's metadata — any or all of its header
copies, its footer, its directory entry's health flags — MUST NOT degrade
classification, mapping, digest validation, or recovery of any other epoch.
At worst the damaged epoch becomes "metadata unavailable"
(`SidecarMetadataUnavailable`, scoped to that epoch by definition). Copy
health is deliberately excluded from the canonical digest so that
*discovering* damage never invalidates the map (Section 7.3).

### 12.6. Terminal Completeness

A normal finalized tape has the exact suffix in Section 8.3 and EOD immediately
after C's trailing filemark. Host durable progress distinguishes the six
barrier-proved boundaries. Planned future tuples in an earlier component never
attest that the later component, its filemark, or its barrier exists.

On a tape presented without host state, a validating C/B/A survivor attests the
complete fixed pre-tail snapshot carried in that replica. Missing or invalid
siblings make the result degraded; disagreement makes it a conflict. No valid
replica invokes the explicit BOT structural walk, whose result is recovery
evidence rather than a fabricated terminal edition. A structural artifact after
the exact terminal suffix is nonconformant and MUST NOT be admitted as an
Object.
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

1. Derive the committed prefix from the off-tape commit records, satisfying
   the authority-agreement rule of Section 3.4, dropping any torn tail, and
   compute `W` and `T` from it.
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
5. Seed the writer with: the complete replayable prefix map, Object recovery
   rows, and one full directory entry for every committed sidecar; the durable
   boundary; `W`; the next monotonic epoch id; and the live
   open-epoch state (`[W, T)`, shape- and CRC-revalidated, then re-accumulated).
   This hardened source must remain replayable so finalization can emit the
   final ParityMap, pre-hash its resulting structural row, and stream the
   identical fixed snapshot into A, B, and C. The incomplete open epoch
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
NoBootstrapFound                BOT identity Bootstrap is absent or invalid
BootstrapParse                  bootstrap frame or payload violates Section 8
BootstrapPayloadTooLarge        framed BOT payload cannot fit the block
SidecarParse                    sidecar structure violates Section 9
SidecarMetadataUnavailable{epoch_id}   no header/index copy validated (Section 13.3)
ParityMapParse                  final pre-tail ParityMap violates Section 10
TerminalIndexReplicaParse       replica framing or fixed-slot payload violates Sections 8/10
TerminalIndexSeparationParse    typed separation extent violates Section 8
TerminalIndexReplicaConflict    independently valid survivors disagree
BotStructuralRecoveryRequired   no terminal replica validates; explicit BOT walk required
SchemeMismatch                  sidecar geometry disagrees with the bootstrap scheme
FilemarkMapDigestMismatch       replica structural projection digest disagrees
FilemarkMapReconstruct          BOT recovery walk could not produce a valid map
OutsideValidatedMapPrefix       refusal: address beyond the validated scope (Section 13.2)
UnrecoverablePendingEpoch       refusal: ordinal ≥ W, parity not yet written
Unrecoverable{stripe, lost_count, limit}   more than m erasures in a stripe
ReedSolomon                     matrix inversion or codec failure
CapacityReserveExceeded         exact terminal/parity reserve is unavailable
ObjectTooLargeForEmptyTape      object plus exact close reserve cannot fit
TerminalRecoveryRequired       failed finalization permits only terminal repair
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
fill, which is excluded from acceptance decisions — Section 8.1); CBOR decoding enforces the Section 5.3 subset; writer-supplied diagnostic text
(bootstrap keys 3 and 4) is bounded in length and charset, and a value
violating either bound is treated as absent and never rendered unescaped
(Section 8.2). The reason for that last bound is that those two fields are
the first human-readable text a diagnostic tool prints from an unknown
cartridge. The operator reading them is deciding whether the cartridge is
damaged or hostile, and the text is chosen by whoever wrote the tape.
Implementations SHOULD fuzz the bootstrap, sidecar, terminal-replica, and separation parsers
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
representation (for example, [REMENCRYPT] envelopes), making every stored
block — and therefore every CRC and parity computation — a function of
ciphertext.

Each terminal replica's fixed 256-byte Object-row slot is the designated
bounded surface for payload-binding recovery metadata. A plaintext REM-OBJECT row exposes manifest location,
manifest size, manifest chunk count, and manifest digest; this is acceptable
because plaintext REM-OBJECT objects are not confidential against a tape reader.
An encrypted REM-OBJECT row exposes only recipient epoch ids, `metadata_frame_len`,
and `key_frame_len`, all already plaintext in the REM-ENCRYPT envelope
stored in the same tape file. The encrypted row MUST NOT carry plaintext
manifest anchors
(`manifest_first_chunk_lba`, `manifest_size_bytes`, `manifest_chunk_count`,
or `manifest_sha256`), because those values describe confidential inner
content and would add leakage beyond the REM-ENCRYPT envelope.

## 17. Test Vectors

The deposited draft.1 archive remains byte-for-byte unchanged and continues to
belong to the published specification. Draft.4 terminal bytes are review-only
candidate vectors under
`fixtures/rem-parity-terminal-index-draft/`; they MUST NOT be copied into or
substituted for publication artifacts before independent review and freeze.

The candidate set contains minimal and multi-object inventories at each legal
record size: 256 KiB, 512 KiB, and 1 MiB. Every profile contains five byte
streams in the exact A/gap-AB/B/gap-BC/C order. Filemarks and EOD are recorded
as structural expectations in `MANIFEST.tsv`, not encoded into those files.
Compact gaps use the Section 8 byte-draft test profile `E = 3*B` (header, one
zero interior record, footer); default one-GiB gaps remain an integration/VTL
obligation.

The generator is
`crates/remanence-parity/examples/generate_terminal_index_vectors.rs`. The
independent verifier `tools/verify_terminal_index_vectors.py` re-derives
HMAC role magics, CRC-64/XZ, full-file SHA-256, header hashes, local
observations, record formulas, component ordering, dense file numbers, logical
positions, zero gap interiors, and terminal EOD without calling the Rust
codec. Candidate bytes remain mutable until the diff gate and an independent
implementation review are complete.

Before freeze, negative candidates MUST cover at least: bad role magic; CRC
failure; reserved/padding nonzero; ordinal/count mismatch; payload slot
truncation; map↔Object-row mismatch; header/footer disagreement; local
observation mismatch; nonzero gap interior; missing filemark; A/B/C survivor
conflict; all three replicas invalid with explicit BOT fallback; and arithmetic
overflow in every size/location formula.
## 18. Conformance and Freeze Criteria

These criteria gate the freeze of this specification. All of them are
satisfied as of this review draft, with the evidence recorded in the Revision
History appendix; they are formally discharged when the document is frozen.
After freeze, revisions are governed by the change policy in the Status of This
Document section: errata and conforming minor revisions are permitted, and
anything that would invalidate an existing tape, change the meaning of
anything already written, or leave an earlier reader unable to identify and
cleanly refuse a newer one is a new major version.
The criteria were:

1. At least one complete implementation implements this document in every
   role — Writer, Scanner, Recoverer, Resumer, Verifier — with no known
   divergences from this document.
2. The Section 17 fixtures are present in the companion archive and pass, including the
   damage matrix and the byte-pinned minimal tape image. Every
   **[pinned-at-generation]** value is independently re-derived by a second
   implementation (different language or library) before freezing, so a
   reference-implementation bug cannot be frozen into the conformance
   anchor.
3. Coverage-guided fuzzing of the bootstrap, sidecar, terminal-replica, and separation
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
- [REMOBJECT] — "REM-OBJECT Core Format", Version 1.0 or any later 1.x revision (published against 1.0.0, DOI of that revision in its Status section), companion specification: the
  reference payload format and plaintext object-row semantics.
- [REMENCRYPT] — "REM-ENCRYPT", Version 1.0 or any later 1.x revision (published against 1.0.0, DOI of that revision in its Status section), companion specification: encrypted
  representation and encrypted object-row field semantics.

CRC-64/XZ is fully parameterized in Section 5.1; no external reference is
required to implement it.

### 20.2. Informative References
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
- Block 0: entries start at 0xC8 (200). All parity entries fit
  (200 + 32,768 = 32,968), followed by
  `(262,136 − 32,968) / 8 = 28,646` data-CRC entries, ending exactly at
  the limit. `inline_index_entry_bytes = 262,136 − 200 = 261,936`.
- Spill block 1: `262,136 / 8 = 32,767` data-CRC entries.
- Spill block 2: the remaining `65,536 − 28,646 − 32,767 = 4,123` entries
  (32,984 bytes), zero-filled below its trailing CRC.

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

A smallest-useful finalized draft.4 tape has the sole BOT Bootstrap followed by
its Object/ParitySidecar prefix and the exact terminal suffix:

```text
file 0   Bootstrap
file 1   Object
file 2   ParitySidecar
file 3   TapeIndexReplica A
file 4   IndexSeparationExtent AB
file 5   TapeIndexReplica B
file 6   IndexSeparationExtent BC
file 7   TapeIndexReplica C
EOD
```

Each replica carries the identical three-row fixed prefix and one Object
recovery row. A Scanner starts from EOD, validates C and survivor agreement,
then exposes that inventory. If C is damaged it tries B, then A. If all three
are invalid it reports terminal authority unavailable and performs the explicit
BOT structural walk; it never treats the tape as empty.
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

Sidecar and terminal-control magics are HMAC(tape_uuid, role label) so that a
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

### B.7. Three complete replicas are separated physically

Each A/B/C member is independently usable: full header, streamed body, local
footer, and trailing filemark. The two typed gaps are physical separation, not
additional index copies. Three complete replicas tolerate the loss of either
end and one middle region without any geometric placement rule. Requiring
surviving editions to agree prevents ordinal preference from hiding a split
authority.

### B.8. Checkpoint authority stays off tape

Open-tape commit state lives in the durable host journals (Section 3.4).
Ordinary checkpoint barriers close any pending parity epoch, prove the covered
files durable, and advance those journals; they do not append a Bootstrap or an
index. On restart, the Writer reconciles the measured physical prefix against
that host authority before it may append. Bare-tape inventory becomes complete
only when finalization writes the three identical terminal replicas. If none
survives, Section 8.4.1 offers structural recovery evidence without inventing
commit authority. This keeps open-tape checkpoint frequency independent of the
number or placement of on-tape index copies.

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

### B.12. The reference off-tape journals are not a media format

The reference implementation uses two append-only per-tape records for a
checkpointed write session. Its Layer 3c tape-file journal
(`<tape-uuid>.remjournal`) is version 4. Version 3 added a
`checkpointed_through` watermark record so ordinary replay can retain,
physically reconcile, and then truncate uncheckpointed orphan bundles before
append. Version 4 adds typed terminal-prefix and terminal-component
transitions with their paired watermarks. Its
checkpoint journal (`checkpoints/<tape-uuid>.remcheckpoint`) records each
synchronized checkpoint's physical EOD and the batch projection needed to
rebuild the catalog.

For parity-enabled sessions these two journals are required commit authority
in the sense of Section 3.4. During ordinary open-tape replay, Layer 3c bundles
beyond the last `checkpointed_through` watermark are orphan evidence: they are
discarded only after physical reconciliation authorizes their removal. Before
an append-positioning `LOCATE`, the reference Resumer compares the journals'
complete checkpointed histories, including tape-file map entries, object
identities, parity watermarks, and terminal EOD. If either checkpointed history
is missing an entry named by the other, is ahead of the other, or conflicts,
resume fails closed.

There is one narrower Finalizing-state exception. For each of the five planned
terminal components, persistence order is: synchronizing media barrier; exact
component record and its immediately following `checkpointed_through` record
in the Layer 3c journal; terminal-progress fsync in the checkpoint journal;
then SQLite projection. Before any terminal positioning or write on restart,
the Resumer reconstructs the immutable final edition and compares every
present terminal-component and watermark digest with that plan. It accepts
only equal component counts or exactly one next canonical Layer 3c transition
ahead. In that single crash window it completes a missing Layer 3c watermark
if necessary, advances the checkpoint journal to that same component, and
rebuilds SQLite, all before media motion. A skip, regression, non-next record,
or conflicting digest fails closed. This exception promotes barrier-proved
terminal progress; it never promotes an ordinary Object or parity bundle.
`AfterReplicaC` remains `Finalizing` until its SQLite progress projection, a
sealed checkpoint containing the exact completed intent, and the final SQLite
projection are durable. On the uninterrupted owner path, sealed-checkpoint
fsync permits retirement of the matching companion before that final SQLite
projection. If interruption instead leaves the sealed checkpoint with its
companion, a recovery lease first verifies exact equality between the sealed
completion and normalized companion. Recovery then projects the sealed
checkpoint while retaining the companion, and retires it only after projection
succeeds. A projection failure therefore preserves host-only retry routing; no
path regresses to in-progress authority. While that companion exists, an
ordinary append owner MUST NOT acquire the checkpoint journal or retire the
companion implicitly; only the explicit terminal-recovery owner may complete
the project-then-retire sequence.

The companion intent format is versioned independently from the tape format.
A companion that carries the durable `RecoveryRequired` classification MUST
use a format revision that older readers cannot silently treat as an ordinary
in-progress intent; unsupported revisions fail closed.

SQLite is a rebuildable projection, not commit authority. A bootstrap-only
SQLite projection with no complete jointly authoritative off-tape commit
record therefore represents an empty committed prefix and does not prevent the
Writer from rewriting file 0 from BOT.

Neither journal format is recorded on tape, and neither changes any
REM-PARITY media byte.

## Appendix C. Draft.1 Historical Closure Record (Informative)

The published draft.1 closed-item snapshot remains in the immutable publication
copy. It described the checkpoint-bootstrap/parity-map design and does not
establish draft.4 conformance. Replacement work is tracked in Appendix E; only
items verified against the terminal triple may be closed in a later preparing
revision.
## Appendix D. Revision History (Informative)

Entries are newest first. Each carries: date · version · kind
(erratum / minor / major / draft milestone) · what changed and its effect on
conformance. Milestones that predate the first published revision are marked
`[draft]`; they were reached in public working drafts, not in published
revisions of this specification, and the change policy of the Status section
governs only the revisions that follow the first published one.

- **2026-08-08 — 1.0.0-draft.3 — review draft.** Defines *committed prefix*
  explicitly in Section 3.1 and clarifies the existing durable-boundary rule
  in Sections 3.4 and 14: when an implementation distributes the logical
  commit record across multiple records that it designates as required commit
  authority, those records must be present, their overlapping claims must
  agree, and their validated combination must yield one complete and
  unambiguous resume state before append positioning or writing. A missing,
  conflicting, incomplete, or ambiguous authority is a `ResumeAppend` failure;
  a rebuildable cache does not commit a tape file.
  Appendix B.12 records how the reference implementation applies that rule to
  its Layer 3c and checkpoint journals, including the empty-prefix treatment
  of an uncommitted bootstrap-only projection.

  This is a clarification of the abstract off-tape commit-record obligation,
  whose storage format remains implementation-defined. It changes no on-tape
  byte, tape-file ordering, Reader behavior, schema value, or pinned vector.

- **2026-08-02 — 1.0.0-draft.2 — review draft.** Resolves Appendix E item
  RP-2. Section 8.2 now bounds bootstrap key 3 to 128 bytes of printable
  US-ASCII and key 4 to 64 bytes of [RFC3339] `date-time`, states the Reader's
  obligation to treat a violating value as absent, and requires escaping
  wherever either value is rendered; Section 16.2 records the same bound among
  its hostile-input posture. Section 10.4 carries the same obligations to the
  parity_map payload's keys 6 and 7, which are the same two fields in the other
  structure that holds them. RP-2 as raised named only the bootstrap keys;
  bounding one pair and leaving the other unbounded would have closed the item
  without closing the hazard.

  This revision also promotes key 3 (and its parity_map counterpart, key 6)
  from OPTIONAL to a Writer MUST, and recommends a matchable
  `<implementation>/<version>` form. RP-2 treated both keys as disposable
  diagnostics. That was wrong about key 3: a tape is produced by an
  implementation and not by a specification, so when a cartridge does not
  decode as this document says it should, the identity of the writing software
  is the only thing that resolves the disagreement — and free text nobody can
  match to a release does not do that job. Readers still tolerate absence, so
  no earlier tape and no other implementation's tape is affected. No tape
  written under draft.1 becomes invalid and
  no Reader outcome changes: both keys were already OPTIONAL, so the treatment
  a violating value now receives is the treatment every conformant Reader
  already gave their absence. Were this a published revision rather than a
  review draft, it would classify as a minor revision under question 3 of the
  Status section's change policy. No pinned vector is affected, and the
  vectors already satisfy the new Writer rule; `schema_minor` is untouched.

- **2026-07-31 — 1.0.0-draft.1 — review draft.** Published for public review;
  not yet frozen. On freezing this becomes version 1.0.0, the first published
  revision, and the change policy in the Status section governs everything
  after it. This revision closes the Appendix C items and satisfies the
  Section 18 criteria; the criteria are formally discharged at freeze, not by
  this draft.

  Relative to the review draft published on 2026-07-25 (see below), this
  revision: replaces the draft Status text with the three-question change
  policy (invalidate or change meaning → major; earlier readers cannot
  identify and cleanly refuse → major; obligations or registry assignment →
  minor under three conditions; otherwise erratum), and states the
  axis-independence principle and the worked classifications; rewrites
  Section 18's preamble, which described the document as a draft and
  permitted only errata after freeze; rewrites Section 8.1.1 as the
  schema-minor registry (Defined-by and wire meanings: 2 = `object_id`
  optional, 3 = `object_id` required) and corrects the stale `1 / 3` pair in
  Section 2.5; states the Reader obligation at `schema_minor` ≥ 3 in
  Section 8.2.1 and the reader rule for unrecognised `flags` bits, both
  matching the reference implementation; adds the semantics-freeze rule to
  Section 5.3; re-anchors the key-30 overflow-carrier prohibition from the
  document version to the registry mechanism; declares the Section 8.4
  discovery-candidate set frozen for the life of `schema_major` 1; corrects
  the parity_map footer magic in Sections 2.5, 5.2 and 10.3, which described
  a `PARITY_MAP_FOOTER_MAGIC_LABEL` that appears in no implementation and on
  no tape — the footer shares the header's magic; corrects the minimum
  sidecar block size from 0xC0 to 0xD0 in Sections 2.5, 9.1 and 9.4 and adds
  the index-packing rejection the Section 9.4 pseudocode omitted; removes an
  unimplementable final-directory redundancy requirement from Section 8.3
  that contradicted Sections 10.1, 10.7 and 11.3; and records in Section 12.4
  the `ParityMapReference`-locator precedence tier and the two-candidate
  requirement for structural discovery, both of which the reference Scanner
  already applied.

  **If you implemented from the review draft, re-check two things.** First,
  Sections 2.5, 5.2 and 10.3 described a `PARITY_MAP_FOOTER_MAGIC_LABEL`
  (`"REM\0PMAPFOOT\x01"`) as the derivation for the `parity_map` footer
  locator's magic. No such label exists: the footer carries the *header's*
  magic, `HMAC(tape_uuid, PARITY_MAP_MAGIC_LABEL)[0..8]`, on every published
  artifact and in every implementation, and it is distinguished from a header
  by its position and by `copy_kind`. An implementation built from the draft
  computes eight wrong bytes and rejects every conformant `parity_map` tape
  file, so this correction restores interoperability rather than removing it.
  Second, the minimum sidecar block size is 0xD0, not 0xC0: sizes 0xC0 through
  0xCF satisfy the header-plus-CRC floor but cannot pack even one index entry,
  and Section 9.4's algorithm now carries the rejection that makes this
  explicit. No tape byte and no published vector changed for either
  correction.

  **Implementation conformance.** A line-by-line audit of this document against
  the reference implementation, the pinned vectors, the repository
  documentation and the project website found six divergences, all of which
  were resolved by correcting the implementation — this text was not weakened
  anywhere. The Scanner no longer aborts a whole catalog-less walk when one
  tape file's recorded block count disagrees with its measured length
  (Section 12.3); a sidecar footer that parses but contradicts the map entry
  now falls back to the primary header rather than stopping (Section 13.3
  step 2); directory-assisted tail rescue is implemented, so an epoch is
  declared metadata-unavailable only when no header/index copy can be
  validated (Section 13.3 steps 3 and 4); geometry and ordinal-range
  disagreements raise `SchemeMismatch` as this document specifies rather than
  a generic parse error; and the REM-OBJECT manifest depth bound is enforced
  through map values as well as arrays. A gate was added that applies each
  damage-matrix cell's recorded faults to the pinned image and drives the real
  recovery path, which is what the earlier tooling never did.

  Two further behaviours were examined and judged conformant rather than
  divergent, and are recorded here so the discharge of criterion 1 is not
  asserted over unexamined ground. First, the bulk region-recovery entry point
  abandons a whole multi-epoch request when one epoch is metadata-unavailable;
  Section 12.5's guarantee is that damage to one sidecar's metadata must not
  degrade *recovery of any other epoch*, and it holds — the per-ordinal and
  per-block paths, which are what the read path uses, recover every other
  epoch normally. The all-or-nothing contract of the bulk call is an
  interface shape, not a recovery-model divergence, and it has no consumer
  outside the parity crate today. Second, Section 8.2 requires a parity
  bootstrap's `scheme_id` to be `rs-cauchy-gf256-v1`; the Reader does not
  reject an unrecognised value at parse time, but refuses cleanly at use time
  with `SchemeMismatch`, naming the scheme found and the scheme expected. The
  requirement binds what a conformant Writer may record; the Reader's
  obligation is to refuse legibly, which it does.

- **2026-07-29 — [draft] pre-freeze revisions II.** Last-resort
  filemark-walk operational envelope specified (Section 8.4.1), with
  inter-file positioning exempted from the Section 8.4 rule-6 abort;
  bootstrap re-typing promoted from SHOULD to MUST with operational
  candidate criteria (Section 12.4) — reader-obligation changes with no
  effect on the set of valid tapes; Appendix C items 3 and 4 resolved.
- **2026-07-25 — pre-release copy.** A copy of this document, marked "Draft
  for review" and dated 2026-06-11, was distributed inside software release
  v1.0.0 (Zenodo 10.5281/zenodo.21551571, a *software* record). It was not
  deposited or citable as a document, carried no DOI of its own, and was
  reachable only by unpacking the source archive. It named Section 18 as the
  criteria still to be met. The vector archive current at
  that time was `b9be8760…`; the archive pinned by the first published
  revision is `77be73e7…`, which adds the REM-OBJECT object-row vectors and
  the independent re-derivation tool without altering any pre-existing
  member. The never-re-pin rule of the Status section binds published
  revisions and does not reach back into the draft era.
- **2026-07-22 — [draft] tape-alone recovery claims.** Ordered persistence
  and the synchronizing barrier made normative on the tape I/O layer
  (Section 3.5); commit discipline extended with batched deferred
  synchronization and staged-record semantics (Sections 3.4, 11.1); the
  attested prefix and the bare-tape tail taxonomy specified with salvage
  rules (Section 12.6); Appendix B.8 reframed to per-file-marker rationale
  plus barrier-grain structural attestation.
- **2026-07-21 — [draft] pre-freeze revisions.** Writer-legal block sizes
  closed over the discovery-candidate set (Section 8.4); object identity
  row keys clarified (Section 8.2.1); epochs redefined as explicit ordinal
  ranges with bare-counter ids and short epochs legalized at any checkpoint
  boundary with `FINAL_PARTIAL_EPOCH` reserved for terminal `finish()`
  (Sections 3.3, 10.5, 11.2); the bootstrap directory ceiling made an
  admission-time refusal with mandatory headroom and seal-at-ceiling
  (Section 8.2.1); reference journal watermark note (Appendix B.12).
- **2026-06-11 — [draft] first draft.** Initial working baseline.

## Appendix E. Open Items (Informative)

This is the live preparing-copy snapshot for the draft.4 replacement.

1. **TT-1 — independent byte derivation.** A second implementation built from
   the terminal byte tables must reproduce every candidate profile and review
   the complete diff before any bytes are frozen.
2. **TT-2 — negative and interruption vectors.** Pin the failures listed in
   Section 17, including each of the five barrier boundaries, disagreement
   between independently valid survivors, and all-replicas-invalid BOT
   fallback.
3. **TT-3 — default-gap media exercise.** Run the exact one-GiB separation
   extents at every legal block size on VTL, and at least two block sizes on
   physical tape, verifying local footer observations and filemark/EOD
   positioning from a clean medium.
4. **TT-4 — end-to-end lifecycle reconciliation.** Exercise the implemented
   sole-BOT/off-tape-checkpoint grammar and prove durable
   `Open -> Finalizing -> Finalized/RecoveryRequired` projection through
   restart, including manual-finalization restart, while showing that Object
   admission cannot reopen after the first finalization record.
5. **TT-5 — external prose review.** Confirm that the completed preparing copy
   contains no normative dependence on geometric placement, `2M+1` index
   copies, bootstrap Object-row ceilings, or singular final Bootstrap
   authority.
## Author's Address

The ArchiveTech Project
Website: https://archivetech.org
Email: specs@archivetech.org
Reference implementation: https://github.com/archivetechie/remanence
