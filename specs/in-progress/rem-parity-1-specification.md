# Rem Tape Parity (REM-PARITY) Format

## Version 1.0 — Specification

| | |
| --- | --- |
| Status | Review draft |
| Document version | 1.0 |
| Version | 1.0.0-draft.5 |
| Date | 2026-09-06 |
| License | CC-BY-4.0 |
| Concept DOI (all revisions of this document) | [10.5281/zenodo.21719156](https://doi.org/10.5281/zenodo.21719156) |
| Reference implementation (informative) | Zenodo concept DOI [10.5281/zenodo.21551570](https://doi.org/10.5281/zenodo.21551570) — software deposit, Apache-2.0 |
| Bootstrap magic | `52 45 4D 00 42 4F 4F 01` (`"REM\0BOO\x01"`, fixed bytes) |
| Erasure scheme identifier | `rs-cauchy-gf256-v1` |

## Status of This Document

This revision defines the generation-2 layout (`schema_major` 2). Tapes of
generation 1 are governed by the published revision 1.0.0-draft.2
(2026-09-06, SHA-256
`1e4ad796bd77291f731d8d2611231984ba778f3650c1597f80eebdc7ba359fd2`) and are
not readable by generation-2 Readers.

**This is a review draft.** It is published for public review and is not yet
frozen. Generation 2 is implemented in the reference tree. Its review-only
candidate vectors for the terminal suffix are pinned and re-derived by the
terminal verifier without calling the Rust codec, and its proof and nonphysical
lifecycle/VTL gates have passed. Dedicated coverage-guided
terminal-replica/separation/parser-walk fuzz plateaus and the supervised
physical-media gate remain open. So does REM-PARITY freeze criterion 2,
recorded in `specs/README.md`. Its evidence is gathered as candidates:
whole-tape images (among them Appendix A.4's minimal tape); damage-matrix,
resume and negative vectors, most of them pinned; and a second implementation,
written from this document, that re-derives the pinned bytes Section 17 lists.
The criterion remains open until the items that Appendix D item TT-2 lists are
closed and the companion archive carries the fixtures at freeze. Candidate
vectors are not publication artifacts until the freeze gates close.

**Comments close on 30 April 2027, and the documents freeze on 31 July 2027**,
one year after publication. On that date the finality promise below takes
effect and the change policy governs every revision after it. The three months
between the two dates exist so that changes made in response to review are
published as such, and visible, before the text is fixed. Comments and defect reports are raised as issues on the project repository.
Before reporting, check the live list of known items at
<https://archivetech.org/spec/issues> — it is current, whereas the Open Items
appendix of this document is only a snapshot taken when this revision was fixed.

On freezing, the format this document defines becomes final: no tape it
validates will ever be invalidated, no Reader guarantee will be withdrawn, and
the discovery guarantee of Section 8.4 will stand for the life of
`schema_major` 2. The document itself may still be revised even then, in
exactly the three ways set out below.

No standards body has reviewed or adopted it. There is no ISO number, no RFC,
no SNIA endorsement. It was written by the same people who wrote the
implementation, so the finality above is our own undertaking and not anyone
else's approval. That is worth saying plainly, because such words usually
imply a committee somewhere, and in this case there is none.

This document has three independent version axes:

| Axis | Value | Where it is recorded |
| --- | --- | --- |
| Document version | 1.0 | This document only; it names no on-tape value |
| `schema_major` | `2` | Bootstrap fixed frame, offset `0x08` (Section 8.1) |
| `schema_minor` | `3` (the current Writer value) | Bootstrap fixed frame, offset `0x0A`; registry in Section 8.1.1 |

None is a proxy for another, and no arithmetic relates them. In particular,
`schema_minor` is not this document's version number; Section 8.1.1 is its
registry. REM-OBJECT §10 and REM-ENCRYPT §10 state those documents' own axes,
among them REM-OBJECT's `REMANENCE.schema_version` (a text feature gate) and
manifest `schema_version` (an integer), and REM-ENCRYPT's registries; none of
them is a proxy for an axis of this document either.

**Which copy governs.** The concept DOI above is reserved, but no revision of
this document has yet been deposited. Until the first deposit is published,
this repository copy is a review draft and not a deposited normative revision.
Once a revision is deposited, its deposited text governs. Every other copy — in the project
repository, inside a Remanence source release, on a mirror, or printed — is a
convenience copy. A copy carrying the same version string as a deposited
revision is byte-identical to it or it is defective; where they differ, the
deposit governs. The version string of a deposited revision is never reused
for different bytes, so naming a deposited version names one exact text no
matter which copy you hold. The
reference implementation is informative: where it and this document disagree,
this document is the fixed point, and the divergence is a defect in the
implementation.

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
   frozen for the life of `schema_major` 2: a scanner that cannot rotate to
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
moves; a new Writer-legal block size — major, by question 2; a new
`schema_major` or magic — major, by definition.

The image-level vectors of Section 17 are conformance anchors of the
revision under which they were generated; a later revision does not make
them retrospectively non-conformant, and never re-pins them. A future
revision that needs new vectors publishes its own archive alongside this
one and cites it separately.

A tape written against this specification will therefore still be read
correctly by an implementation built from any later revision of it. A Reader
does not need to know which revision wrote a tape in order to read it; the
Revision History appendix exists so that anyone can recover what a given
revision's text said, not to identify Writers.

Errata are raised as issues on the project repository. Every published
revision of this document is archived with its own DOI and recorded in the
Revision History appendix.

This document remains the normative fixed point for the format: an
implementation is validated against it, not the reverse. The arithmetic test
vectors here (CRC, Reed–Solomon, canonical digest) are normative and can be
re-derived from this text alone; the image-level vectors of Section 17 are
*pinned-at-generation*, and the archive holding them is published with a
checksum so you can confirm you have the same bytes we do.

## Abstract

This document specifies the Rem Tape Parity (REM-PARITY) format, version 1.0:
a self-describing, parity-protected method of writing multiple archival
Objects to linear tape. Objects are opaque byte strings — this format never
interprets their content — written one per filemark-delimited tape file and
protected collectively: a fixed **bootstrap** at the beginning of the tape
identifies the tape and its geometry; **parity sidecar tape files** carry
Reed–Solomon parity and per-block CRCs for each parity epoch; a **ParityMap**
tape file, written before the terminal suffix, carries the sidecar epoch
directory when that directory is nonempty; and exactly three complete
**terminal replicas**, separated by two one-GiB **separation extents**, carry
the final structural map and the Object recovery rows. Parity is computed over
GF(2⁸) with a Cauchy generator (`rs-cauchy-gf256-v1`), incrementally
accumulated as data streams, and written strictly in separate tape files, so
every Object remains a clean, contiguous run of blocks readable with standard
positioning tools. The format is designed for catalog-less recovery: a reader
holding only this document, a damaged tape, and a generic SHA-256/HMAC/CBOR
toolkit can reconstruct the tape's structure, verify it cryptographically, and
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
10. [Final ParityMap, Terminal Replicas, and Separation Extents](#10-final-paritymap-terminal-replicas-and-separation-extents)
11. [Writer Obligations](#11-writer-obligations)
12. [Scanner Obligations](#12-scanner-obligations)
13. [Recoverer Obligations](#13-recoverer-obligations)
14. [Resumer Obligations](#14-resumer-obligations)
15. [Errors](#15-errors)
16. [Security Considerations](#16-security-considerations)
17. [Test Vectors](#17-test-vectors)
18. [Freeze Criteria and Status](#18-freeze-criteria-and-status)
19. [IANA Considerations](#19-iana-considerations)
20. [References](#20-references)

Appendix A. [Worked Examples (Informative)](#appendix-a-worked-examples-informative)  
Appendix B. [Design Rationale (Informative)](#appendix-b-design-rationale-informative)  
Appendix C. [Revision History (Informative)](#appendix-c-revision-history-informative)  
Appendix D. [Open Items (Informative)](#appendix-d-open-items-informative)  
[Author's Address](#authors-address)

---

## 1. Introduction

### 1.1. Purpose and Design Goals

REM-PARITY defines how a sequence of archival Objects is laid out on one
linear tape and how that layout survives media damage. Its design goals, in
priority order:

1. **Catalog-less recovery.** Everything needed to map, verify, and repair a
   tape lives on the tape. Off-tape state (catalog, journal) accelerates
   recovery but is never required for it.
2. **Payload independence.** Objects are opaque byte strings. The format
   never reads object content to classify, map, verify, or repair a tape;
   any archiving tool whose Objects meet the Section 4 contract can write
   and recover conformant tapes.
3. **Clean coexistence with standard tooling.** Parity and self-description
   live in separate, filemark-delimited tape files. An Object occupies its
   own tape file as a contiguous run of fixed blocks, navigable with
   standard positioning tools (`mt fsf` + read); no parity byte ever appears
   inside an Object.
4. **Bounded damage tolerance with bounded memory.** The default geometry
   tolerates a contiguous burst of up to `S × m` blocks (~512 MiB) per epoch —
   **including bursts that straddle the data→sidecar boundary** (Section 9.1,
   Appendix B.2) — while the Writer holds only `S × m` parity accumulators
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

A REM-PARITY tape is a sequence of filemark-delimited tape files. While open
it contains the bootstrap, Objects, and sidecars. Final parity closeout may
append one ParityMap immediately before the normal finalization appends the
terminal suffix. The labels in the diagram abbreviate terms defined in
Section 2.3.

```text
| bootstrap(0) | object | sidecar | ... |
| index A | gap AB | index B | gap BC | index C | EOD
```

A Writer appends Objects one per tape file. As object blocks stream to tape,
the Writer accumulates Reed–Solomon parity over them in fixed-size **stripes**
grouped into **epochs**; each completed epoch's parity and per-block CRCs are
written as a **parity sidecar** tape file at the next object boundary. Epochs
span Objects: the parity geometry is independent of object sizes, and an
Object needs no minimum size to be protected. The bootstrap records tape
identity, fixed block size, and parity scheme. There are no intermediate tape
indexes and no singular final bootstrap. Each terminal replica contains the
complete canonical structural map plus exactly one recovery row per Object. A
healthy bare-tape inventory reads BOT identity, then reads the terminal
replicas from EOD and accepts one that validates and agrees with every other
valid replica (Section 8.5), without an Object walk. If all replicas are
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

This document cites its own sections as “Section 8.2” and a section of a
companion specification by the companion's name, as in “REM-OBJECT §4.5.1” or
“REM-ENCRYPT §5.2”.

### 1.4. Non-Goals

This format performs no encryption and no authentication (Section 16.1) —
confidentiality and authenticity of object content belong to the object
format. It does not define capacity or placement *policy* (only the policy's
wire consequences), does not define the commit-store, journal, audit, or
catalog formats, and does not support multiple tape partitions (all positions
are partition 0).

### 1.5. What this document specifies, and what it does not

This document defines a format: how its bytes are laid out, what they mean, and
what may be concluded from them. Its definitions, registries, change policy and
test vectors serve that purpose. It also contains rules about what an
implementation does. Each of those rules is included only because ignoring it
would do one of three things:

1. It would produce bytes that another conformant reader cannot read, or would
   misread.
2. It would let two conformant readers reach different conclusions about what a
   medium or an object contains.
3. It would let a tool claim something that the bytes do not support.

Conforming to this document is therefore necessary for interchange, but it is
not enough for safe operation. This document does not say how a tool should hold
keys, stage and publish what it writes, protect the computer it restores onto,
organise its work after a crash, or report progress to its operator. It does say
what the bytes a tool writes must be, and what a tool may claim about the bytes
it reads.

Recommended practice for those other matters is collected in the REM
Implementation and Operations Guide
(<https://github.com/archivetechie/remanence/blob/main/docs/rem-implementation-guide.md>).
The Guide is informative and is revised independently of this document, which
can be implemented without it. Descriptions of the reference implementation, and
the record of how revisions of this document are prepared and frozen, are kept
in the repository that holds the reference implementation and these
specifications; this document does not depend on them.

## 2. Conventions and Terminology

### 2.1. Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174]
when, and only when, they appear in all capitals, as shown here.

A paragraph that opens with *Rationale.* is informative and states no
requirement.

Where a numbered list sets out steps, they are taken in the order given unless
the text says otherwise.

### 2.2. Conformance Roles

A single implementation may fill several roles.

- **Writer**: produces parity-protected tapes (Section 11).
- **Scanner**: reconstructs the filemark map from a bare tape (Section 12).
- **Recoverer**: reconstructs damaged data blocks from parity (Section 13).
- **Resumer**: re-opens a committed tape for append (Section 14).
- **Verifier**: validates a tape's structures and digests end to end without
  recovering payload — the Scanner's checks plus the Recoverer's index and
  CRC validation. It reports damage it finds before the terminal suffix with
  the error a Reader reports for that component. A sidecar copy or footer that
  reads and violates Section 9 is `SidecarParse`. Damage that a Reader
  survives without reporting an error is still damage, and the Verifier
  reports it without a Section 15 name: a sidecar copy or footer, a copy of
  the ParityMap, or a terminal replica whose records cannot be read, even when
  another copy or replica is used, and a bootstrap that is unreadable while
  supplied values are used. A ParityMap footer that cannot be read, or that is a
  record of the wrong length, is reported in the same way. A ParityMap footer that reads and is invalid rejects the
  ParityMap (`ParityMapParse`, Section 10.1.4), and the Verifier reports that
  error. A medium error on such a component is reported as
  an unreadable component. A non-medium fault of the transport stays `TapeIo`.
  A Verifier's validation is
  a full verification: it reads every data block that a sidecar protects and
  every parity shard, and checks each against its sidecar's index
  (Section 13.4). It reports each block or shard that fails by its address:
  a data block's tape-file position, or a parity shard's epoch, stripe and
  parity index. It reads those blocks as opaque bytes and does not interpret
  Object content (Section 1.1, goal 2). A check of structure and metadata
  alone, which reads no data block or parity shard, is not a full
  verification. When no header/index copy of an epoch validates, there is no
  index to check against. The Verifier still reads every data block that the
  map says the sidecar protects and every parity shard that the map entry
  locates (`H = (total − 1 − P) / 2`, Section 13.3), reports each read
  failure, and states that the blocks and shards it read could not be checked
  against a CRC. It does not report a block as failed for lack of a CRC. An
  index that fails the pin of Section 13.3 is likewise not used for CRC
  checks: the Verifier reports the pin's error and the read failures, and
  takes the parity shards from the acquired index. It reports the terminal
  suffix as complete when all three replicas and both separation extents
  validate in full and EOD follows C's trailing filemark (Section 12.6). It
  reports the tape as complete when the terminal suffix is complete, the full
  verification was performed, and it finds no failed data block, no failed
  parity shard and no finding about a sidecar or about the prefix. A finding
  about a copy of the ParityMap or its footer likewise leaves the tape not
  complete. The
  Verifier measures and classifies every tape file of the prefix, including
  when the replicas validate, and reports the damage it finds there. A
  Verifier does not report whether a stripe could be recovered. It reports a
  nonzero fill of the bootstrap (Section 8.1) only when the bootstrap can be
  read.
- **Reader**: any role that reads a tape: the Scanner, the Recoverer, the
  Resumer and the Verifier, and the Writer when it reads back what it has
  written. A requirement on a Reader binds each of them. This is not the
  REM-OBJECT Reader, which reads an object.
- **Inventory Consumer**: the role that receives a Scanner's inventory
  (Section 12.4). It is distinct from the REM-OBJECT Consumer.

### 2.3. Definitions

- **Block / block size**: the tape's fixed block size; one value per tape,
  recorded in the bootstrap. All structures in this document are sized in
  blocks of this one size. All tape I/O is in whole blocks.
- **Tape file**: a run of one or more fixed blocks delimited by exactly one
  trailing filemark. Kinds: **object**, **parity sidecar**, **bootstrap**,
  **ParityMap**, **terminal replica**, **separation extent**.
- **Object**: one opaque archival byte string occupying exactly one object
  tape file (Section 4). The capitalised word “Object” names this term;
  attributive uses, such as “object tape file”, “object bytes” and
  “object block index”, are lower case.
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
- **Terminal suffix**: the five tape files that follow the final prefix:
  terminal replica A, separation extent AB, terminal replica B, separation
  extent BC and terminal replica C, in that order (Section 8.3).
- **Terminal replica**: one of the three tape files A, B and C of the terminal
  suffix, each carrying the complete terminal inventory (Sections 10.2 to
  10.4).
- **Replica envelope**: the header and footer records of a terminal replica
  (Section 10.4).
- **Separation extent**: one of the two tape files, AB and BC, that keep the
  terminal replicas physically apart; each is a header record, zero-filled
  interior records and a footer record (Section 10.5).
- **Filemark map**: the tape's structural table of contents — one entry per
  tape file (Section 7).
- **Sidecar epoch directory**: the per-epoch repair root that the final
  ParityMap carries (Section 10.1.1).
- **Terminal inventory**: the immutable dense structural map and matching
  Object recovery rows repeated in full by A, B, and C (Sections 10.2 and
  10.3).
- **Edition**: the immutable facts that terminal replicas A, B, and C share:
  every field of kind *common* in the replica frame (Section 10.4). Two
  replicas of one edition differ only in their replica-local fields.
- **Implicit zero**: a logical data position beyond a short epoch's real
  data — an all-zero shard that is never written to tape
  (Section 6.4).

### 2.4. Integer, Byte, and Text Conventions

All multi-byte integers in sidecar and terminal structures are
**little-endian**, except the bootstrap fields explicitly marked
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
(Section 16.2). Here, overflow means that a value's exact integer value does
not fit, as the next paragraph defines.

Every formula in this document denotes its exact integer value. A Reader
rejects a value that is recorded, or used as a position, a count or an
ordinal, when its exact value does not fit the field or the u64 that holds
it; the error is the one named for the structure that carries the value. A
value that is only compared is compared exactly, and is never too large. An
intermediate overflow in one way of writing a formula is not a format
violation, and a Reader MUST NOT reject for it. The same rule applies to
values from off-tape commit records, which are `ResumeAppend` when they do
not fit (Sections 3.4 and 14), and to positions a device reports, which are
`TapeIo` when they do not fit or when they go backwards: the device, not the
tape, is then at fault. A limit of the host, such as an allocation bound, is
an implementation limit and not a format violation.

### 2.5. Constants

| Constant | Value |
| --- | --- |
| `BOOTSTRAP_MAGIC` | `52 45 4D 00 42 4F 4F 01` (`"REM\0BOO\x01"`, fixed bytes) |
| `BOOTSTRAP_SCHEMA_MAJOR` | 2 — generation 2; Readers MUST reject any other value (Section 8.1) |
| `BOOTSTRAP_SCHEMA_MINOR` | 3 — the reference Writer's current value, not a conformance bound; see the Section 8.1.1 registry |
| `BOOTSTRAP_HEADER_LEN` | 0x38 |
| `FLAG_NO_PARITY` | bit 0 of the bootstrap flags |
| `MAX_BOOTSTRAP_SCAN_BLOCKS` | 1024 |
| Block-size discovery candidates | 256 KiB, 512 KiB, 1 MiB (Section 8.4) |
| `SIDECAR_MAGIC_LABEL` | `"REM\0PAR\x01"` (8 bytes: `52 45 4D 00 50 41 52 01`) |
| `SIDECAR_FOOTER_MAGIC_LABEL` | `"REM\0PARFOOT\x01"` (12 bytes: `52 45 4D 00 50 41 52 46 4F 4F 54 01`) |
| `PARITY_MAP_MAGIC_LABEL` | `"REM\0PMAP\x01"` (9 bytes) |
| Terminal replica header / footer magic labels | `"REM\0TIREP\x01H"` / `"REM\0TIREP\x01F"` (11 bytes each: `52 45 4D 00 54 49 52 45 50 01 48` / `… 01 46`) |
| Index separation header / footer magic labels | `"REM\0TISEP\x01H"` / `"REM\0TISEP\x01F"` (11 bytes each: `52 45 4D 00 54 49 53 45 50 01 48` / `… 01 46`) |
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
| Minimum frame sizes | bootstrap 0x40; ParityMap 0xC8; sidecar 0xE0; replica 0x400; separation 0x200 |

## 3. Tape Model and Address Spaces

### 3.1. Tape Files

A REM-PARITY tape is a sequence of tape files numbered densely from 0 at the
beginning of tape (BOT), each terminated by exactly one filemark, followed by
end of data (EOD). The labels in the diagram abbreviate terms defined in
Section 2.3.

```text
| bootstrap(0) | object | sidecar | ... |
| index A | gap AB | index B | gap BC | index C | EOD
```

Tape file 0 MUST be a bootstrap. This format owns every filemark on the tape.
An Object's bytes MUST NOT depend on filemarks for internal structure. A
Writer MUST NOT emit filemarks except as tape-file terminators. A tape file
MUST contain at least one block. An immediate filemark is structural damage.

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
  of media), which is why drive compression is rejected on a parity tape
  (Sections 8.4, 11.4).
- **`ParityDataOrdinal`** (u64): the dense numbering of object data blocks
  only, in tape order, skipping filemarks and all non-object tape files. For
  an object tape file whose first block has ordinal `F`, block `b` of the
  file has `ordinal(b) = F + b`. Non-object files have no ordinals; parity
  shards have no ordinals. First-ordinals are dense and contiguous: in tape
  order, each object file's first ordinal equals the previous running total
  of object blocks, starting at 0.
- **Object block index**: the zero-based block index within one Object's
  tape file — the address an object format uses internally.
  `(tape_file_number, object_block_index)` resolves to an ordinal through
  the filemark map (Section 7).

### 3.3. Ordinal-to-Stripe Mapping

For scheme `(k, m, S)` (Section 6.6), locate the unique sidecar descriptor
whose explicit range `[start, end)`, of at most `S × k` data ordinals,
contains ordinal `o`. Let `d = o − start`; then:

```text
epoch       = descriptor.epoch_id
o_in_epoch  = d
stripe      = d mod S                  (the stripe index varies fastest)
data_index  = d / S                    (0 ≤ data_index < k)

inverse:  o = start + data_index·S + stripe
```

The Recoverer uses the inverse to decide whether a stripe position holds a
real data shard or an implicit zero (Section 13.4). That decision compares the
exact value of `o` with the protected end, as Section 2.4 requires, so a
position whose `o` would not fit in u64 is an implicit zero, and is not
rejected.

The interleave is essential: `N ≤ S` physically consecutive data blocks land
in `N` distinct stripes, so contiguous damage of up to `S × m` blocks stays
within the per-stripe tolerance `m` (Appendix B.2). Parity shards are
addressed `(epoch, stripe, parity_index)` with `0 ≤ parity_index < m`. They
live in sidecar tape files (Section 9), never among the data blocks. A sidecar
stores them **parity-index-major** (Section 9.1), so consecutive parity blocks
belong to consecutive stripes and the same interleave extends into the parity
region. A contiguous burst that straddles the data→sidecar boundary therefore
spreads across stripes on both sides, and the guarantee holds across the
boundary, not only within object data (Appendix B.2).

### 3.4. The Durable Boundary

A tape file is **committed** only when all of the following hold, in order:
its blocks and its trailing filemark are written; blocks and filemark are
**synchronized to medium** — by a synchronous filemark, or by a later
synchronizing barrier (Section 11.1) completing before the commit record; and
a durable off-tape **commit record** exists. The commit record's format is
implementation-defined (a journal, a database row, a replicated log entry).
There is no on-tape commit marker and no on-tape "unclean" marker: an
interrupted tail simply lies beyond the last committed file and is physically
superseded on resume (Section 14). Tape files are written and numbered
strictly sequentially (next = last committed + 1, first = 0, at most one in
flight). A Recoverer seeded from a *prefix*-scoped map (Section 7.4) MUST
treat rows beyond the validated prefix as forensic only, never as recovery
inputs.

An implementation MAY store the contents of this logical commit
record in more than one durable off-tape record. Before a Resumer positions to
an append point or writes, the validated combination of the records it relies
on MUST determine exactly one committed prefix and its append point. If a
record the Resumer relies on is missing, their claims conflict, or their
combination is incomplete or ambiguous, resume MUST fail as `ResumeAppend`. A
record the implementation does not treat as commit authority, such as a
rebuildable catalog or cache, does not commit a tape file.

Recommended practice for commit records is described in the REM Implementation
and Operations Guide, under “Writing tapes: sessions, commit and resume”.

Finalization has a separate irreversible lifecycle:

```text
Open -> Finalizing -> Finalized
                    \
                     -> FinalizedDegraded
                    \
                     -> RecoveryRequired
```

The accepted transition to `Finalizing` permanently disables Object admission.
Finalization is not a pause: no failure, restart, recovery action, or degraded
acceptance may transition the tape back to `Open`.

A component failure or completion-unknown result enters or retains
`RecoveryRequired`. From there a Writer may repair only missing tape files of
the terminal suffix, at proved locations. It MUST NOT write an Object or
append a second terminal suffix. A Writer MUST NOT report a tape as
`Finalized` while fewer than three complete replicas exist. A distinct audited
operator action MAY accept a proved set of one or two complete replicas as
`FinalizedDegraded`. This document does not define that action. Zero complete
replicas, an unproved position, or completion-unknown cannot be accepted;
three complete replicas use ordinary `Finalized`. A finalized-degraded tape
cannot resume finalization or return to `Open`.

Recommended practice for finalization is described in the REM Implementation and
Operations Guide, under “Finalizing a tape and recovering from a crash”.

### 3.5. Requirements on the Tape I/O Layer

Fixed-block reads and writes only; a read returning other than exactly one
block is an error, with two classified boundary outcomes: **Filemark** and
**EndOfData**. The tape I/O layer MUST distinguish the Filemark and EndOfData
outcomes on every transport, and on a SCSI transport in both of its sense-data
formats [LTO-SCSI].

A read that returns a record shorter or longer than one block reports a fact
about the tape, not a failure of the device: the record on tape is not one
block long. A Reader treats it as invalid content of the component it belongs
to: an invalid candidate for a control component, an erasure when the
Recoverer reads a data block or parity shard (Section 13.4), and, for the first
record with a supplied or known block size, the refusal of Sections 8.4 and
15. It is never `TapeIo`, which is reserved for a transport or medium failure
and for the device reports that Section 2.4 names. The first record in that
rule is the first record of the tape. A record of the wrong length at the head
of any later tape file is invalid content of that file: the file is measured
by filemark spacing, as for an unreadable head (Section 12.3), and the record
occupies one logical position, as every record does. The
tape I/O layer therefore reports a record of the wrong length, with its
measured length, as an outcome of its own, distinct from both boundary
outcomes and from a transport failure.

The tape I/O layer MUST persist blocks and filemarks to medium strictly in
submission order. The tape I/O layer MUST provide a **synchronizing barrier**:
an operation whose successful completion proves that every block and filemark
issued before it is on medium. A zero-count synchronous SCSI WRITE FILEMARKS
is one such operation. A completion-unknown outcome at a barrier MUST be
treated as barrier failure. On a transport that cannot guarantee ordered
persistence, the attestation conclusions of Sections 3.4 and 12.6 are void.

## 4. The Object Contract

### 4.1. Objects Are Opaque Block Strings

An **Object** is a byte string whose length is a positive exact multiple of
the tape block size. It is written as exactly one object tape file of
`block_count = length / block_size` contiguous blocks followed by one
filemark. This format places no constraint on the bytes themselves: any
block content is valid, and nothing in this format ever parses object bytes.
A Writer MUST refuse an Object whose length is not a positive multiple of the
block size. Padding an Object up to a block multiple is the responsibility of
the payload format or the caller. The padding becomes part of the Object's
bytes, indistinguishable from content to this format.

### 4.2. What the Format Provides to Objects

For every **protected** object block — every block whose ordinal is below
the watermark `W`, i.e. whose epoch's sidecar has been emitted — this
format holds a CRC-64/XZ of the block in that sidecar (Section 9.3) and
protects the block with Reed–Solomon parity (Section 6) at the epoch's
geometry. Between sidecar emissions, fewer than one epoch's worth of
committed data is pending (`W ≤ ordinal < T`, Section 11.2): durable, but
not yet repairable, and recovery refuses it as such (Section 13.2).
Finalization (Section 11.3) closes the final epoch, so on a finalized tape
every object block is protected. The format provides:

1. **Structural addressing.** `(tape_file_number, object_block_index)`
   resolves through the filemark map to a `ParityDataOrdinal` and back, so
   damaged-block reports and recovered blocks are exchanged in either
   address space.
2. **Block-granular detection.** A block whose stored CRC mismatches is
   detected without any payload-format knowledge.
3. **Bounded repair.** Up to `m` damaged blocks per stripe are
   reconstructed from any `k` surviving shards (Section 13), keyless and
   content-blind — an encrypted Object is repaired without its keys.
4. **Commit semantics.** An Object is durable exactly when its tape file is
   committed (Section 3.4); the Writer reports completion only then.
5. **Catalog-less rediscovery.** A Scanner classifies object tape files by
   elimination — never by reading object content (Section 12.3) — so a tape
   of foreign Objects is mappable by any conformant implementation.

This format provides integrity and recovery at **block** granularity only.
End-to-end content integrity (per-file digests, manifests) and
confidentiality are the payload format's job (Section 16.4).

### 4.3. What the Format Requires of Payload Formats

A payload format carried as REM-PARITY Objects:

1. MUST produce Objects whose stored length is a positive exact multiple of
   the tape block size (Section 4.1).
2. MUST NOT require filemarks, tape positioning side effects, or any medium
   feature inside an Object: an Object round-trips as a plain byte string
   through any storage that preserves bytes.
3. MUST tolerate that a reader is handed whole blocks.

Payload bindings MAY add bounded descriptive rows
through the terminal Object-row surface (Section 10.3), with the
leakage constraints stated in Section 16.4.

A payload format is easier to recover from a tape when it is self-framing: it
carries its own end-of-content structure, so its content length can be
recovered from its own bytes. It is easier to trust when it is self-describing
and carries its own content-level integrity, such as per-file digests, because
this format verifies blocks, not meaning. When catalog-less payload recovery
matters, it helps if a payload format makes its Objects identifiable from their
own bytes, with a magic or a header, because this format's generic structures
identify *which tape files are objects* but do not parse object bytes.

### 4.4. Payload Format Bindings (Informative)

**REM-OBJECT [REMOBJECT].** A REM-OBJECT meets the contract by construction:
its stored bytes are an exact positive multiple of its `chunk_size` in both
representations (plaintext and encrypted); with the tape block size equal to
`chunk_size`, one REM-OBJECT stored block is one tape block. Parity is
computed over stored bytes — ciphertext when the Object is encrypted — so
damaged encrypted Objects are repaired keyless and opening under [REMENCRYPT]
is retried on the recovered bytes.

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
tape identity from the bootstrap or, when the bootstrap is unreadable, from the
tape UUID supplied under Section 8.4.1, `LABEL` is the role's ASCII label from
Section 2.5 (label bytes exactly as listed, including the embedded NUL, the
0x01 version byte, and, for the terminal replica and separation labels, the
role letter that follows it; no terminator added), and `[0..8]` takes the first
8 bytes of the 32-byte MAC. The label bytes never appear on tape. Each block
role has a distinct label, except that the ParityMap header and footer share
one label; Sections 10.1.2 and 10.1.3 define how the two are told apart. The
bootstrap magic alone is a fixed byte string, because a Reader does not yet
know the tape UUID when it searches for the bootstrap (Section 8.1). Derived
magics are an *identity* mechanism — these blocks belong to this tape and role
— not authentication (Section 16.1).

### 5.3. Deterministic CBOR

All CBOR frame payloads in this format are single definite-length,
integer-keyed maps in RFC 8949 deterministic encoding: shortest-form
integers and lengths, map keys sorted in ascending order of their
deterministic encodings — compared first by encoded length, then
lexicographically by encoded bytes, as [RFC8949] Section 4.2.1 specifies —
no duplicate keys, no tags, no floats, no indefinite-length items. The Section 7.3 canonical-digest preimage is not a
frame payload or a map: it applies these canonical-encoding rules to its
specified array-of-arrays structure. A terminal replica's structural slot
(Section 10.2) is likewise an array, not a map: it carries one Section 7.3
per-entry array under the same rules. Each payload map MUST occupy its entire
declared payload extent (for the bootstrap, `cbor_payload_len`, Section 8.1).
Bytes after the map's definite-length encoding, within the declared payload,
are a nonconformity. When a Reader decodes any CBOR item of this format, it
MUST reject duplicate keys and non-canonical encoding, and MUST ignore unknown
integer keys at every map level. Ignoring unknown keys is the format's
extension mechanism. A future revision of this document may assign new keys;
it never changes the meaning of existing ones, and a change that would do so
requires a new schema version or magic. An assigned key MUST NOT alter an
existing field's meaning, the recovery outcome of any tape written without it,
or any other rule this document enforces. An extension that would do so is a
new major version.

*Rationale.* A Reader built from an earlier revision ignores the key, so it
would recover different results with no wire signal.

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
byte-identical to the definitions above (REM-PARITY freeze criterion 6,
recorded in `specs/README.md`).

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
accumulation: `accumulate(i, shard)` XORs `G[j][i] ⊗ shard` into each of `m`
zero-initialized accumulators. Incremental and batch encodings MUST be
byte-identical.

*Rationale.* This is what lets a Writer stream object data without buffering
an epoch (Section 11.2).

### 6.4. Implicit Zeros

In a short epoch, logical data positions beyond the real data are all-zero
shards that are *never written to tape* and never accumulated (an all-zero
shard contributes nothing to any parity accumulator). The sidecar's
`real_data_shard_count` versus `logical_shard_count = S × k` tells a Reader
which positions are implicit (Section 9.2). The product `S × k` is only
compared, as Section 2.4 says of a compared value, and is never too large.
Implicit-zero positions are never
erasures: the Recoverer supplies an all-zero block for them during
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

The scheme triple is recorded in the bootstrap and in every sidecar; Readers
MUST use the recorded values or, when the bootstrap is unreadable, the values
supplied under Section 8.4.1, never defaults. Under this bound `S × k`, `S ×
m` and `S × (k + m)` each fit in 32 bits, so these three products never
overflow; an overflow can arise only in a formula that also uses a value read
from tape. The profiles below are
informative Writer defaults, with `S` chosen as
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
kinds 4 and 5 inside that payload are invalid, even though a Scanner
recognizes those kinds in the measured physical tail. Every record position
and every trailing filemark position that the map describes, by Section 3.2's
`LBA(f, b)`, MUST fit in u64; a recorded map that breaks this is invalid. The
append position after the last filemark is not a validity rule of the map; it
is a Writer and Resumer concern (Sections 3.4 and 14). A walked map's
positions are device reports, so for it Section 2.4's `TapeIo` applies.
Derived scalars:

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
no replica-health state. The replica envelopes separately bind the complete
planned layout of the terminal suffix, while the replicas' canonical payload
remains the prefix before A. Discovering damage therefore changes health
evidence, not the canonical inventory.

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

In each replica envelope, the digest travels with its scope:

| Field | Meaning |
| --- | --- |
| `sha256` | the canonical digest |
| `tape_file_count` | the prefix length (number of leading tape files) the digest covers |
| `map_total_data_ordinals` | `T` over that prefix |
| `highest_protected_ordinal` | `W` over that prefix |
| `is_final_map` | true for a terminal final-prefix inventory |

When a Reader validates a terminal digest, it MUST recompute the digest over
exactly the leading `tape_file_count` entries and cross-check all three
scalars. A terminal digest yields a **Complete final-prefix** map. The tape
files of the terminal suffix are checked against the separate planned layout
and observed placement; they are not recursively embedded in the payload map.
Recovery MUST be fenced to the validated scope (Section 13.2).

## 8. BOT Bootstrap and Terminal Index

The bootstrap is the tape's UUID-independent entry point: a single block at
tape file 0, findable by magic, that records the tape's identity, block size,
and parity scheme. It is the only structure in this format with a fixed
(non-derived) magic, because a Reader does not yet know the tape UUID when
searching for it.

A generation-2 tape carries no later copy of the bootstrap. Every generation-2
bootstrap is Object-count independent: its payload contains no Object recovery
rows, including on a no-parity tape. Host checkpoint operations do not emit a
bootstrap. Payload key 2 carries only the BOT-only digest record of
Section 8.2 and is not terminal inventory authority. Keys 20, 21, and 30 are
reserved and MUST be absent (Section 8.2). A Writer MUST NOT publish a
checkpoint or final index through the bootstrap payload. Final structural and
Object rows live only in the streamed terminal replicas defined in
Sections 8.3, 10.2, and 10.3.

### 8.1. Fixed Frame (One Block, Exactly)

A bootstrap tape file is exactly one block:

| Offset | Len | Field | Type | Constraint |
| --- | ---: | --- | --- | --- |
| 0x00 | 8 | magic | fixed bytes | `BOOTSTRAP_MAGIC` |
| 0x08 | 2 | schema_major | **u16 BE** | MUST be 2; Readers reject ≠ 2 |
| 0x0A | 2 | schema_minor | **u16 BE** | registry in Section 8.1.1; Readers accept any value; payload rules MAY gate on it |
| 0x0C | 4 | flags | **u32 BE** | bit 0 = no-parity. Writers MUST zero all other bits. Readers MUST ignore bits they do not recognise. A future flag may therefore carry only semantics an earlier Reader can safely ignore; anything stronger requires a new `schema_major` |
| 0x10 | 16 | tape_uuid | raw bytes | the tape's identity (16 opaque bytes; RECOMMENDED a version-4 UUID [RFC9562], unique per tape); the HMAC key of Section 5.2 |
| 0x20 | 4 | block_size_bytes | **u32 BE** | MUST equal the size of the block it was read with |
| 0x24 | 8 | sequence | **u64 BE** | exactly 0 for the sole bootstrap |
| 0x2C | 4 | cbor_payload_len | **u32 LE** | payload byte length |
| 0x30 | 8 | crc64_header | **u64 LE** | CRC-64/XZ over bytes 0x00..0x30 |
| 0x38 | var | CBOR payload | Section 8.2 | |
| +len | 8 | crc64_payload | **u64 LE** | CRC-64/XZ over the payload bytes |
| … | | zero fill to block end | | MUST be written zero; not an acceptance rule (see below) |

The endianness mix — big-endian header integers, little-endian length and CRCs
— is fixed. An implementation MUST NOT "normalize" it (Appendix B.5). The
minimum viable block size is 0x40 (header plus the payload CRC of an empty
payload). Parse order: block length ≥ 0x40 → magic → header CRC → schema_major
→ payload bounds (checked against the block) → payload CRC → CBOR.

Writers MUST zero the trailing fill. Verifiers MUST verify it and report a
nonzero fill as a nonconformity. A Reader MUST NOT let the fill decide whether
it accepts a bootstrap, in discovery (Section 8.4) or in classification
(Section 12.3). This is the one deliberate exception to the Section 16.2
verify-zero rule.

*Rationale.* A damaged fill byte then cannot cost the tape its entry point.

#### 8.1.1. Schema Minor Registry

`schema_minor` names generations of the bootstrap wire format. It is not a
revision counter and not this document's version number: no arithmetic
relates it to anything, most revisions of this document assign no new value,
and a value is assigned only when a wire-visible change warrants signaling,
by the document revision that defines the change.

This registry covers `schema_major` 2. The registry for `schema_major` 1 is
that of the published revision 1.0.0-draft.2 (2026-09-06, SHA-256
`1e4ad796bd77291f731d8d2611231984ba778f3650c1597f80eebdc7ba359fd2`).

| `schema_minor` | Status | Defined by | Wire meaning |
| ---: | --- | --- | --- |
| 0–2 | never assigned under `schema_major` 2 | — | none; these values carry meaning only under `schema_major` 1 |
| 3 | **current Writer value** | REM-PARITY 1.0 | no terminal-index semantics; Object rows are carried by terminal replicas |
| ≥ 4 | unassigned | a future revision of this document | a value here indicates a tape written under a later revision; retrieve the current revision via this document's concept DOI |

A Reader accepts any value in the fixed frame. Individual future payload rules
MAY be gated on the value. A Writer SHOULD emit the current value. The
unchanged publication archive contains major-1 images at minor values 2 and 3;
those bytes are not generation-2 vectors, and a major-2 Reader rejects their
narrow bootstrap authority.

*Rationale.* Accepting any value is deliberate. It differs from REM-ENCRYPT's
`format_version`, whose unassigned values are hard errors. An unassigned value
is not an error here: Section 5.3's ignore-unknown rule means a Reader reads
through newer bootstrap revisions, satisfying the change policy's second
condition outright. The refuse-with-typed-error condition governs formats that
gate reading on a registry, which the bootstrap does not.

To identify what defines a tape in hand: read `schema_major` and
`schema_minor` from the bootstrap (Section 8.1) — each assigned value's
defining revision is named in this registry; read the Object recovery rows for
the identities and geometry of what is stored (Section 10.3). Every revision
of this document is retrievable through its concept DOI. Which revision
*wrote* the tape is not recorded on the wire and is not needed for reading;
treat `schema_minor` as provenance only where this registry gives it a wire
meaning.

### 8.2. CBOR Payload

A single integer-keyed map (Section 5.3):

| Key | Type | Presence | Meaning |
| ---: | --- | --- | --- |
| 1 | map | REQUIRED on a parity bootstrap; absent on a no-parity bootstrap | scheme record: `{1: tstr scheme_id, 2: uint k, 3: uint m, 4: uint S}` |
| 2 | map | REQUIRED unless no-parity | BOT-only digest record: `{1: bytes .size 32 sha256, 2: uint tape_file_count=1, 3: uint map_total_data_ordinals=0, 4: uint highest_protected_ordinal=0, 5: bool is_final_map=false}` |
| 3 | tstr, ≤ 128 bytes | OPTIONAL | writing-implementation identity; printable US-ASCII only |
| 4 | tstr, ≤ 64 bytes | OPTIONAL | [RFC3339] write timestamp |
| 5 | bool | REQUIRED. A Writer MUST write it. A Reader MUST treat its absence as false | `drive_compression` — effective hardware compression at session open. `true` on a parity bootstrap MUST be rejected (Sections 8.4, 11.4) |
| 20 | — | reserved; MUST be absent | reserved |
| 21 | — | reserved; MUST be absent | reserved |
| 30 | — | reserved; MUST be absent | reserved |

Any of keys 20, 21, or 30 in a generation-2 bootstrap is a parse error. Those
keys carried generation-1 structures (an inline sidecar epoch directory, a
ParityMap reference, and REM-OBJECT object rows), whose keys are defined in
Section 8.2 of the published revision 1.0.0-draft.2 (2026-09-06). A
**no-parity bootstrap** (flag bit 0 set) marks a tape written without parity
protection. It MUST omit the scheme record (key 1) and MAY omit the digest
record (key 2). A Reader MUST NOT require those records on it. A no-parity
bootstrap's payload carries no parity scheme; one that does is
`BootstrapParse`. With supplied values, Section 8.4 refuses such a bootstrap
when its header is valid and the supplied scheme is a parity scheme, because
the no-parity flag disagrees. When the supplied scheme is no parity, it refuses
the bootstrap if the scheme record can still be decoded, and otherwise treats
the bootstrap as unreadable. For a parity bootstrap
the scheme record's `scheme_id` MUST be `rs-cauchy-gf256-v1` and `(k, m, S)`
MUST satisfy Section 6.6 validity. Unknown keys are ignored at every level
(Section 5.3).

No Reader decision defined by this document depends on key 3 or key 4.

Key 3 identifies the software that wrote the bootstrap, as at most 128 bytes
drawn from printable US-ASCII (`0x20`–`0x7E`). Key 4 records when the
bootstrap was written, as at most 64 bytes forming a valid [RFC3339]
`date-time`.

A Reader MUST tolerate the absence of either key, and MUST treat a value
violating either rule exactly as it treats that key's absence, for every
purpose. It MUST NOT refuse the bootstrap, the tape file, or the tape on
account of either.

Recommended practice for descriptive fields is described in the REM
Implementation and Operations Guide, under “Descriptive fields”.

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

The payload of A, B, and C describes the complete prefix before A and excludes
the terminal suffix. The three payloads are identical; the headers and footers
of the three replicas differ only in their replica-local fields
(Section 10.4). Each replica uses one full header record, one or more payload
records, and one full local footer record (Section 10.4). Each default
separation extent includes its header and footer within
`ceil(1 GiB/block_size)` records (Section 10.5). The shared planned layout is
computed before A. Local observations in a footer MUST equal that plan.
Planned future components never prove their existence.

Unless a field is explicitly inside deterministic CBOR, every fixed-width
integer in the terminal structures is unsigned little-endian, including the
two-byte slot-length prefix; deterministic CBOR uses its own canonical
big-endian argument encoding. Arithmetic on these fields is checked in `u64`
(Section 2.4). Terminal replicas and separation extents are written and read
only at the record sizes 256 KiB, 512 KiB, and 1 MiB; an out-of-band read hint
chooses one of those sizes and does not extend the terminal layout
(Section 8.4). The terminal layout uses partition 0 and requires hardware
compression disabled. On a tape without parity this requirement stands as it
does on any other tape: a replica or separation frame that records another
compression mode is invalid (Section 10.6), and key 5 of the bootstrap, which
records the mode at session open, does not decide it.

The planned layout contains:

- `partition: u32`;
- `block_size: u32`;
- exactly five ordered component tuples; and
- `expected_eod_lba: u64`.

Each component tuple is exactly 32 bytes:

| Offset | Size | Field |
| ---: | ---: | --- |
| `0x00` | 2 | structural kind: `4` replica or `5` separation |
| `0x02` | 2 | one-based ordinal within its kind |
| `0x04` | 4 | trailing filemark count, exactly `1` |
| `0x08` | 8 | planned dense tape-file number |
| `0x10` | 8 | planned logical start LBA |
| `0x18` | 8 | data-record count before the filemark |

The order and ordinals are fixed: `(4,1), (5,1), (4,2), (5,2), (4,3)`.
Tape-file numbers are dense. Logical start positions advance by
`record_count + 1`, where the `1` is the actual trailing filemark. EOD is the
start of C plus C's record count plus one filemark. Every component has at
least two records, the three replicas have equal record counts, and the two
separation extents have equal record counts.

Each 32-byte component tuple is little-endian:

```text
kind:u16 || ordinal:u16 || trailing_filemark_count:u32=1 ||
planned_tape_file_number:u64 || planned_start_lba:u64 || record_count:u64
```

The layout digest is:

```text
SHA256(
  "REM-TERMINAL-TAIL-LAYOUT-V1\0" ||
  partition:u32 || block_size:u32 || component_count:u16=5 ||
  five 32-byte component tuples || expected_eod_lba:u64
)
```

This digest contains only planned on-media facts. A conservative filemark
capacity charge is not an on-media location and is deliberately excluded.

### 8.4. Discovery (Reader)

A Scanner with no off-tape state first reads the bootstrap at BOT to establish
tape identity, block size, and parity geometry. It then positions to EOD and
discovers the terminal replicas:

1. locate, from EOD, the local footer of a terminal replica, and from it the
   planned terminal layout (Section 8.3); the Scanner spaces back over up to
   five filemarks from EOD and reads the record before each, stopping early
   when a backspace does not cross exactly one filemark or leaves the
   partition. A terminal replica's footer that parses supplies a planned
   layout only when its recorded footer position equals the position at which
   it was read, and the layout's planned EOD is at or after the tape's EOD. A
   layout whose planned EOD lies before the tape's EOD may be followed by
   later tape files, so it is not used, and when no footer supplies a layout,
   no replica validates;
2. validate the header, footer and trailing filemark of every replica that
   layout plans; a replica's payload need be validated only for the replica to
   be accepted, or when two replica envelopes differ in an edition-common
   field;
3. before accepting any replica, consider every replica that validates for
   conflict under Section 8.5;
4. if no replica validates, perform the BOT structural recovery walk in
   Section 8.4.1.

When footers propose different planned layouts, Section 8.5 decides which
replicas are accepted. The Scanner reads the footer before every filemark it
spaces back over, and does not stop at the first that supplies a layout. Only
the footer of a terminal replica supplies a layout: the footer of a separation
extent carries the same planned tuples and does not. A footer that supplies no
layout, because of its position or its planned EOD, proposes nothing, and the
replicas of that layout are not compared under Section 8.5. A replica is fully
valid (Section 8.5) only in a layout that step 1 supplies. A tape file written
after a terminal suffix, or a second terminal suffix, moves the tape's EOD
past the planned EOD of the first suffix, so that layout is not used. Unless a
footer supplies another layout whose replicas validate, no replica validates
and the Scanner takes the walk of Section 8.4.1. The first backspace from EOD
crosses any records that follow the last filemark, as each later backspace
crosses the records of a file, and stops at the nearest filemark before the
current position.

A Scanner that cannot read the bootstrap, and is given the three values
Section 8.4.1 names, MUST perform the discovery above with them. It then uses
the supplied tape UUID as the key of the role magics (Section 5.2). A readable
bootstrap whose values disagree with the supplied ones is refused; the
supplied values never take its place. Without supplied values, a Scanner that
finds no usable bootstrap reports `NoBootstrapFound` and ends discovery,
because no tape UUID is then available to key the role magics (Section 5.2). A
Scanner does not take the tape UUID from the plaintext field of a terminal
replica or of a separation extent.

With supplied values, the Scanner judges the first record in two stages: the
physical read first and then, for a record of the right length, its content.
The bootstrap's fixed header is 0x38 bytes, and its header CRC covers the
first 0x30 of them; the payload carries its own CRC.

| Reading the first record gives | The Scanner |
| --- | --- |
| a medium error, or a filemark or EOD where the record should be | treats the bootstrap as unreadable and continues on the supplied values |
| a transport failure | reports `TapeIo` |
| a record whose measured length differs from the supplied block size, shorter or longer | refuses it (`BootstrapParse`, naming the block size) |

The comparison is always with the supplied block size. A Scanner that also
reads the first record at a candidate size other than the supplied one does
not refuse for a mismatch with that candidate; it refuses only when the
record's measured length differs from the supplied size. The refusal names the
block size because it compares the record's measured length with the supplied
size. That refusal is made from the measured length alone, before the frame's
`block_size_bytes` is read.

For a record of the right length, the first matching row applies:

| Content | The Scanner |
| --- | --- |
| the magic is missing, or the header CRC fails | treats the bootstrap as unreadable |
| the header CRC is valid, and the format major, tape UUID, block size, sequence (which Section 8.1 fixes at 0) or no-parity flag is impossible or disagrees with the supplied values | refuses it (`BootstrapParse`, naming the first such field in that order), even when the payload is damaged |
| the header is valid, and the payload CRC fails or the payload's length runs past the block | treats the bootstrap as unreadable |
| both CRCs are valid, and the payload breaks a later rule of Section 8 | treats the bootstrap as unreadable, unless a value that can still be decoded disagrees, as in the next two rows |
| a decodable `drive_compression` is true, and the tape is a parity tape | refuses it (`DriveCompressionEnabled`) |
| a decodable parity scheme disagrees with the supplied scheme | refuses it (`BootstrapParse`, naming the scheme) |

A bootstrap that is treated as unreadable is not trusted for any value. A
bootstrap whose label is imperfect but agrees with the supplied values
therefore leaves the tape recoverable, and a bootstrap that contradicts them
is refused. A value can still be decoded when the payload is a well-formed
CBOR map from which the value can be read, whether or not the payload meets
the canonical-encoding rules of Section 5.3. A breach of those rules is one of
the later rules of Section 8 in the fourth row of the second table. When the
supplied scheme is no parity, a parity scheme record in the payload of a
no-parity bootstrap is a value that disagrees in this sense: the Scanner
refuses the bootstrap (`BootstrapParse`, naming the scheme), as Section 8.2
requires. When the supplied scheme is a parity scheme, the second row of the
second table has already refused such a bootstrap. A refusal ends discovery
with that error, and no inventory is returned. A bootstrap that is treated as
unreadable does not end discovery: the Scanner continues on the supplied
values. In the second table, a field is impossible when it breaks its
constraint in Section 8.1: a major other than 2 or a sequence other than 0.

Footer-local observed positions MUST agree with the footer's planned shared
layout before that replica is eligible. A planned position or digest for a
later component does not prove that the component was written. Filemarks and
EOD are structural evidence and are not represented as payload bytes.

All layout locations are committed plans calculated before terminal tape
motion. Each footer records the Writer's local file, start, and record-count
observation and the header hash. A Reader locates the frame from the immutable
planned tuple, checks device-reported post-read positions for the addressed
records and trailing filemark, and cross-checks the footer observation against
that tuple. The footer's tape-file, start, and count fields are Writer
observations, not an independent device tape-file counter. A valid footer
still does not prove its trailing filemark or any host-side commit record.

When the block size is unknown, a Scanner MUST apply each discovery candidate
(256 KiB, 512 KiB, 1 MiB) as a real drive reconfiguration before reading. It
accepts a parsed bootstrap only if its `block_size_bytes` equals the
configured read size.

A conformant Writer for production media MUST use one of the
discovery-candidate block sizes. This closes the Writer-legal set over the
discovery set: every conformant tape is discoverable from the media alone,
with no out-of-band hint. A Scanner MUST accept an operator-supplied
block-size hint and apply it as a configured read size. The hint path serves
damaged-media recovery and nonconformant tapes, not Writer freedom.

The generation-2 terminal vectors use the same 256 KiB, 512 KiB, and 1 MiB
record sizes as conformant media. The frozen publication archive contains
historical 4096-byte major-1 images; those bytes are verified by the isolated
publication tools and are not terminal-suffix vectors. An operator may still
supply another size to the separate BOT structural walk for damaged or
nonconformant media, but that hint cannot make a terminal replica or
separation extent at that size eligible.

Section 10.6 defines the exact local eligibility conditions. A magic or CRC
miss invalidates that candidate. A medium error invalidates the affected
candidate, while a non-medium transport error aborts discovery. A record of
the wrong length invalidates the candidate in the same way (Section 3.5).
`drive_compression = true` on a parity bootstrap still rejects the tape
(Sections 11.4, 16.3).

#### 8.4.1. All-Replicas-Invalid BOT Walk

When A, B, and C are all absent or invalid, the Scanner MUST offer a full
structural walk from BOT. The bootstrap supplies tape identity and geometry
when it is readable. If it is unreadable, the expected tape UUID, the block
size and the parity scheme (`k`, `m` and `S` of `rs-cauchy-gf256-v1`, or no
parity) supplied out of band are required.

*Rationale.* The identity hint is required because terminal, ParitySidecar,
and ParityMap frame magics are derived from the tape UUID; geometry alone
cannot derive, validate, or safely classify those frames.

The walk reconstructs tape-file boundaries, validates recognisable
ParitySidecar and control structure, and measures complete Object candidates
by elimination. It reports each candidate's identity as unknown unless a
separate exact Object-recovery authority succeeds. Such authority MAY be a
surviving fsynced host checkpoint journal or a representation-aware REM-OBJECT
recovery pass. A rebuildable catalog projection alone is insufficient. The
walk does not invent a terminal replica. The Scanner MUST report that terminal
authority was not recovered. A terminal replica that the walk finds intact is
a tape file of the walk and is not Object authority: it supplies no Object
identity.

- **Optional Object authority.** Before emitting a recovered identifier, the
  Scanner MUST bind the authority to the expected tape UUID and block size,
  prove a committed structural-prefix boundary, and prove a bijection between
  its strictly ordered Object rows and every measured complete Object below
  that boundary. Each row MUST match the measured tape-file number and stored
  block count and carry a 1–64-byte non-NUL Object identifier. Complete
  Objects at or beyond the authority boundary remain unknown. A torn Object is
  incomplete and MUST NOT inherit an authority row. Missing authority is not
  an error. The Scanner MUST fail closed on conflicting, corrupt, or
  non-repeatable authority rather than emit a guessed identity.

- **Identity and geometry hints.** A Scanner MUST accept an expected tape UUID,
  a block size and a parity scheme (`k`, `m` and `S` of `rs-cauchy-gf256-v1`,
  or no parity) supplied out of band. Expected tape UUID, block size and
  parity scheme are mandatory when the bootstrap is unreadable. A block-size
  hint makes the size known and is applied as a configured read size under the
  Section 8.4 hint path, suppressing candidate rotation. Hints MUST NOT cause
  any tape file to be skipped.
- **Termination.** The walk ends at EOD. Encountering EOM first is reported as
  truncation and feeds the completeness outcomes of Section 12.6 unchanged.

Recommended practice for announcing the walk, reporting its progress and
stopping it is described in the REM Implementation and Operations Guide, under
“Reading tapes”.

### 8.5. Authoritative Selection

A terminal replica is *fully valid* when it is locally eligible under
Section 10.6. A Scanner MUST NOT accept a replica while another fully valid
replica differs from it in any edition-common field: any row of kind *common*
in the replica frame of Section 10.4, which together are the replicas'
*edition* (Section 2.3). A disagreement in any edition-common field is
`TerminalIndexReplicaConflict` and is never resolved by ordinal preference; a
Scanner MUST NOT choose one side of a conflict merely because it is newer in
the terminal suffix. A missing or invalid replica is degraded evidence, not a
conflict, and does not invalidate an agreeing survivor. If no replica
validates, selection yields the explicit BOT structural recovery path of
Section 8.4.1. A conflict is not that case: the Scanner returns
`TerminalIndexReplicaConflict` and no inventory, and this document does not
require the walk after it.

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

Parity shard blocks are raw blocks with no headers. Physical block placement
is determined solely by the locator above from a shard's explicit
`(stripe_index, parity_index)` fields; it is **independent of the order of
that shard's entry in the Section 9.3 index stream** (which remains
stripe-major). A Reader MUST locate a parity shard by computing
`block(stripe, parity_index)` from its explicit fields, never by the position
of its index entry. The tail copy MUST carry metadata and index content
identical to the primary; only `copy_kind` and the recomputed CRCs differ.
When both copies are valid and their canonical metadata hashes (Section 9.5)
differ, a Recoverer MUST use a copy that the footer or the sidecar epoch
directory vouches for and neither contradicts, and MUST reject both when
neither is available (Section 13.3). A Verifier MUST read every sidecar's
primary copy, tail copy and footer, and MUST report a divergence between the
copies as `SidecarParse`, even when the footer or the directory decides it.
The minimum block size for sidecars is 0xE0 — block 0 must hold the 0xC8-byte
header, at least one 16-byte parity index entry, and the trailing 8-byte CRC.

### 9.2. The Header Block (Block 0 of Each Copy) — All Little-Endian

| Offset | Len | Field | Constraint |
| --- | ---: | --- | --- |
| 0x00 | 8 | magic | HMAC(tape_uuid, `SIDECAR_MAGIC_LABEL`)[0..8] (Section 5.2) |
| 0x08 | 16 | tape_uuid | MUST match the bootstrap or, when the bootstrap is unreadable, the tape UUID supplied under Section 8.4.1 |
| 0x18 | 8 | epoch_id u64 | |
| 0x20 | 2 | k u16 | ≠ 0 |
| 0x22 | 2 | m u16 | ≠ 0 |
| 0x24 | 4 | S u32 | ≠ 0 |
| 0x28 | 4 | block_size u32 | MUST equal the actual block size |
| 0x2C | 4 | schema_version u32 | MUST be 2 |
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
`[protected_ordinal_start, protected_ordinal_end_exclusive)`. The first epoch
starts at ordinal 0; subsequent epoch ranges MUST be contiguous, with each
start equal to the preceding end. Epoch ids MUST increase by one but carry no
range arithmetic. `real_data_shard_count` MUST equal
`protected_ordinal_end_exclusive − protected_ordinal_start` and MUST be in
`1..=S × k`. A value below `S × k` marks a short epoch whose missing logical
positions are implicit zeros (Section 6.4). Short epochs are legal at any
checkpoint boundary, including mid-tape. A header whose
`sidecar_total_block_count` differs from `2H + P + 1`, with `H` and `P` its
own fields, is invalid.

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
0..block_size−8, and unused space below the CRC MUST be zero. A Reader MUST
verify each of those CRCs, and an index block whose CRC does not verify makes
its copy invalid (Section 13.3).

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
therefore carry the same hash, and the sidecar epoch
directory (Section 10.1.5) can verify a surviving header copy independently of
*which* copy survived. Readers MUST verify the hash on every index parse.

### 9.6. The Footer Block (Last Block) — All Little-Endian

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

The footer's `tape_uuid` MUST match the bootstrap or, when the bootstrap is
unreadable, the tape UUID supplied under Section 8.4.1, as each header copy's
`tape_uuid` must (Section 9.2). The magic alone does not check it, because the
magic is derived from the tape UUID the Reader already holds. The footer's
`sidecar_total_block_count` MUST equal `2H + P + 1`, with `H` and `P` the
footer's own fields. A footer that breaks this equation, or whose tail start
is not `H + P`, violates Section 9.

The footer is a *locator*: it holds everything needed to find and check
either header copy without reading the other, and it sits at the end of the
tape file where it is found by reading the file's last block (Section 12.3
item 6, Section 13.3).

## 10. Final ParityMap, Terminal Replicas, and Separation Extents

This section defines the control tape files that finalization writes and the
payload that every terminal replica carries: the final ParityMap and its
sidecar epoch directory (Section 10.1); the structural rows (Section 10.2) and
Object recovery rows (Section 10.3) of the fixed-slot payload; the replica
frame and its digests (Section 10.4); the separation extents (Section 10.5);
and the conditions under which a replica is valid (Section 10.6). Section 8.3
places these files on the tape.

### 10.1. The Final ParityMap

#### 10.1.1. Purpose

The **sidecar epoch directory** is the per-epoch repair root: for each
sidecar it records where it is, what range it protects, its layout counts,
and its `canonical_metadata_hash` — enough to locate and verify a surviving
header copy even when scan-time classification of that sidecar failed
(Section 13.3 step 3).

When final parity closeout has a nonempty sidecar epoch directory, the Writer
emits exactly one `rem-parity-map-v1` ParityMap before replica A, and that
ParityMap carries the directory.

The ParityMap is parity-closeout metadata and one structural row in the fixed
pre-A prefix; it is not terminal inventory authority. A generation-2 Writer
MUST NOT emit intermediate ParityMaps, checkpoint indexes, or a singular final
index. The complete authoritative inventory is the fixed-slot payload repeated
in full by replicas A, B, and C.

For the final ParityMap, the Writer MUST set `canonical_map_digest` to the
SHA-256 digest of the Section 7.3 canonical projection of tape files 0 through
the ParityMap's own entry, inclusive. The Writer MUST set
`directory_scope_tape_file_count` to the ParityMap's tape file number plus one.
The Writer MUST set `directory_scope_total_data_ordinals` to that prefix's
`T` at finalization (Section 7.2). The Writer MUST set
`directory_scope_highest_protected_ordinal` to that prefix's `W` at
finalization. These definitions apply to the header/footer fields and their
matching payload values (Sections 10.1.4 and 10.1.5).

#### 10.1.2. Copy Layout

With payload length `L`, block size `B`, and `M = ceil((0xC8 + L) / B)` blocks
per copy:

```text
blocks 0 .. M−1       primary copy   (header at offset 0, payload from offset 0xC8)
blocks M .. 2M−1      tail copy      (identical content; copy_kind differs)
block  2M             footer locator
total = 2M + 1
```

The payload's bytes run contiguously from offset 0xC8 of the copy's first
block across the copy's subsequent blocks with no per-block framing; the
copy's unused tail MUST be zero. There are **no per-block CRCs** in a
ParityMap: payload integrity is the header's `payload_sha256`, and
redundancy is the dual copy plus the footer locator. The minimum block size
for ParityMap files is 0xC8.

#### 10.1.3. Header and Footer Blocks — All Little-Endian

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

`block_size` MUST equal the tape's block size, which is the length of every
block read, as the sidecar header's `block_size` must (Section 9.2).

A Reader MUST verify the CRC-64/XZ at 0xC0 of each header copy and of the
footer, and MUST NOT rely on a block whose CRC does not verify. Such a header
copy or footer is invalid. A header copy whose block cannot be read is invalid
in the same way. When one header copy is invalid and the other is
valid, the valid copy is used; when both are valid they MUST agree
(Section 10.1.4). A footer that reads and is invalid rejects the ParityMap
(`ParityMapParse`). A footer that cannot be read, or that is a record of the
wrong length, does not reject it: the header copies are used as above, and
Section 2.2 says how a Verifier reports each case.

#### 10.1.4. Payload (CBOR)

A single integer-keyed map (Section 5.3):

```text
{1: tstr  "rem-parity-map-v1",
 2: bytes .size 16   tape_uuid,
 3: uint  sequence,
 4: map   SidecarEpochDirectory          (Section 10.1.5),
 5: bytes .size 32   canonical_map_digest,
 6: ?tstr writer_version,                 (≤ 128 bytes, printable US-ASCII)
 7: ?tstr write_timestamp}                (≤ 64 bytes, RFC3339)
```

Keys 6 and 7 are the ParityMap's counterparts to bootstrap keys 3 and 4
(Section 8.2): key 6 is at most 128 bytes of printable US-ASCII, and key 7 at
most 64 bytes of [RFC3339] `date-time`. A Reader MUST
tolerate the absence of either, MUST treat a violating value exactly as that
key's absence, and MUST NOT refuse the ParityMap on account of either.

Readers MUST validate the locator arithmetic (`M` from `payload_len` and
`block_size`; total = 2M + 1; the three start indices) and reject
disagreement between header, footer, and the measured tape file length. The
agreement between a header copy and the footer covers every field that both
carry, other than `copy_kind` and the CRC, not only the locator fields.

The decoded payload MUST match the header/footer locator fields (UUID,
sequence, digest, scope, and `is_final_directory`) and the payload bytes MUST
hash to `payload_sha256`. When the primary and tail copies both parse, they
MUST agree: every header field other than `copy_kind` and the CRC, and every
payload byte, is equal in both, and a disagreement rejects the ParityMap.
`sequence` is a per-tape counter over ParityMap emissions, independent of the
bootstrap sequence; beyond the agreement rules above, no Reader decision
defined by this document depends on its value.

#### 10.1.5. The SidecarEpochDirectory (CBOR)

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
 9: uint .size 4 flags}                     (u32)
```

Flags: 0x01 = `FINAL_PARTIAL_EPOCH`; 0x02 = primary-copy-known-good; 0x04 =
tail-copy-known-good. Unknown flag bits MUST be rejected.

**Writer rules.** In this revision a Writer MUST set the directory's
`is_final_directory` (key 4) to `true` (CBOR 0xF5), and the ParityMap header
and footer byte at 0x90 to 1. The directory's unprotected range
`[scope_highest_protected_ordinal, scope_total_data_ordinals)` is empty,
because finalization closes the partial epoch before it writes the ParityMap
(Section 11.3). A Writer MUST set `FINAL_PARTIAL_EPOCH` only on the short
epoch closed by the finalization sequence (Section 11.3), and MUST leave it
clear on a short epoch closed by a checkpoint barrier.

**Reader rules.** Whenever a Reader decodes a sidecar epoch directory, it MUST
validate all of the following invariants; a violation is `DirectoryInvalid`
(Section 15). A Reader MAY trust a directory without reading the sidecar
headers (Section 10.1.1). The invariants below therefore restate at decode
time what Section 9.2 requires of the on-tape sidecar-header sequence:

- entries strictly ascending by `tape_file_number`, each `< scope_tape_file_count`;
- non-zero `sidecar_total_block_count`, `sidecar_header_block_count`, and
  `parity_shard_block_count`;
- `scope_highest_protected_ordinal ≤ scope_total_data_ordinals` (the range
  `[scope_highest_protected_ordinal, scope_total_data_ordinals)` is the tail of
  committed-but-unprotected ordinals permitted by Section 11.2);
- the protected ranges **partition `[0, scope_highest_protected_ordinal)`**:
  taken in ascending `tape_file_number` order, the first
  `protected_ordinal_start` is `0`, each entry's `protected_ordinal_start` equals
  the previous entry's `protected_ordinal_end_exclusive` (contiguous, no gaps and
  no overlaps), every range is non-empty, and the last
  `protected_ordinal_end_exclusive` equals `scope_highest_protected_ordinal` (or
  the directory is empty and `scope_highest_protected_ordinal = 0`);
- `epoch_id` values are unique and consecutive starting from `0`
  (`0, 1, …, count−1`), matching the bare monotonic epoch counter of Section 2.3.

An unknown flag bit is likewise `DirectoryInvalid`. A directory that is not
well formed — a missing key, a value of the wrong type, or a `flags` value that
does not fit in 32 bits — is `ParityMapParse`, not `DirectoryInvalid`. A
Reader accepts key 4 as `true` or `false` and rejects any other value, and
accepts the header and footer byte at 0x90 as 0 or 1 and rejects any other
value. Readers MUST NOT treat an unflagged short epoch as invalid.

### 10.2. Structural Rows

The payload begins with exactly `structural_row_count` 64-byte slots in dense
tape-file order. Every payload slot, structural or Object, has one shape:

```text
encoded_len:u16 || deterministic CBOR || zero padding to the fixed slot size
```

`encoded_len` is little-endian, nonzero, and at most the slot size minus two;
it counts exactly the bytes of the one deterministic CBOR item that follows,
and every byte after that item in the slot is zero. Each structural slot
carries one structural entry, encoded as the seven-element array of
Section 7.3, in that order, with an absent optional field encoded as `null`
(0xF6):

```text
[tape_file_number:u64,
 kind:u64,
 block_count:u64,
 first_parity_data_ordinal:?u64,
 protected_ordinal_start:?u64,
 protected_ordinal_end_exclusive:?u64,
 epoch_id:?u64]
```

Kinds 0, 1, 2, and 3 mean Object, ParitySidecar, Bootstrap, and the optional
final ParityMap. Structural row 0 MUST be the kind-2 bootstrap that
Section 3.1 requires at tape file 0. A final edition therefore always has at
least one structural row. Kinds 4 and 5, which identify terminal replicas and
separation extents, MUST NOT occur in this payload. Its scope ends immediately
before A. Every structural and ordinal-range invariant is validated.

### 10.3. Object Recovery Rows

After all `structural_row_count` structural slots, every terminal replica
carries exactly `object_row_count` 256-byte fixed slots, one for each kind-0
structural row, in strictly increasing `tape_file_number` order — the same
tape-file order as the structural rows. Each slot has the shape of
Section 10.2 and carries the deterministic CBOR of the following
integer-keyed map:

| Row key | Type | Presence | Meaning |
| ---: | --- | --- | --- |
| 1 | uint | REQUIRED | filemark-delimited object `tape_file_number` |
| 2 | tstr | REQUIRED | representation marker: `"plaintext"` or `"encrypted"` |
| 3 | uint | REQUIRED | stored block count for the object tape file |
| 4 | bytes, 1–64 | REQUIRED | REM-OBJECT `object_id`, the identity the archive answers "where is object X" with, carried verbatim as its 1–64 non-NUL bytes. Any REM-ENCRYPT envelope NUL padding (REM-ENCRYPT §5.2) is stripped. The value matches REM-OBJECT §4.5.1 exactly: opaque UTF-8, 1–64 bytes, with no conversion. |
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

Key 10 (`manifest_first_chunk_lba`) is the zero-based index, *within the
Object's tape file*, of the first chunk of the manifest entry's payload: a
REM-OBJECT inner `BodyLba`, not a Section 3.2 Logical LBA. In a plaintext copy
one REM-OBJECT chunk is one tape block (Section 4.4). Key 11 is the manifest's
payload byte length, key 12 its chunk count, and key 13 the SHA-256 of its CBOR
bytes. Key 21 is the REM-ENCRYPT header's `metadata_frame_len`; key 22 records
the recipient epoch ids present in its key frame; key 23 is the header's
`key_frame_len`. The semantics of these three keys, and the key 21 and key 23
bounds, are defined by [REMENCRYPT], which is a normative reference for
implementations of terminal Object recovery rows. REM-ENCRYPT requires distinct
`recipient_epoch_id` values of every key frame and forbids a Sealer to use an
all-zero id (REM-ENCRYPT §5.3); this document requires the same of the
recipient epoch ids in an Object recovery row's key 22: they are distinct, and
none is all zero.

*Rationale.* A catalog-less scan must tell recipient slots apart.

Plaintext rows MUST carry keys 10–13 and MUST NOT carry keys 21–23.
Encrypted rows MUST carry keys 21–23 and MUST NOT carry keys 10–13. For
plaintext rows, `manifest_chunk_count` and `manifest_size_bytes` MUST be
positive, the manifest chunk range MUST fit within `stored_block_count`, and
the manifest byte length MUST fit within `manifest_chunk_count ×
block_size_bytes`. For every row, `stored_block_count` MUST be positive and
MUST match the structural filemark-map row for key 1.

The row set is the complete final-prefix Object inventory. Its count MUST equal
the number of kind-0 structural slots, and the ordered `(tape_file_number,
stored_block_count)` pairs MUST be a bijection with those slots.

There is no one-block Object-row ceiling. A tape's close reserve is the space
its finalization needs: parity closeout, the checked terminal payload
`64 × structural_row_count + 256 × object_row_count` in three rounded replica
records, both separation extents, and five filemark charges.

Recommended practice for capacity admission is described in the REM
Implementation and Operations Guide, under “Capacity admission”.

The structural fields of Section 10.2 — tape-file numbers, block counts,
data and protected ordinals, and epoch ids — and the Object rows' tape-file
numbers, stored-block counts, and plaintext manifest positions, sizes, and
counts decode as `u64`. The encrypted representation's bounded key-frame
length retains its REM-ENCRYPT `u32` constraint.

### 10.4. Terminal Replica Framing and Digests

**Record geometry.** Let `B` be the fixed record size:

```text
payload_len            = 64 × structural_row_count + 256 × object_row_count
payload_record_count   = ceil(payload_len / B)
payload_padding_bytes  = payload_record_count × B − payload_len
footer_block_offset    = 1 + payload_record_count
replica_record_count   = 2 + payload_record_count
```

One replica tape file is:

```text
one full header record
payload_record_count full payload records
one full footer record
one trailing filemark
```

The header never shares a record with payload bytes. The replica footer is a
record of the kind-4 tape file; it is not a kind-2 bootstrap, and there is no
bootstrap after C. The payload is all 64-byte structural slots in tape-file
order (Section 10.2) followed by all 256-byte Object recovery-row slots in
matching Object order (Section 10.3). It runs contiguously across the payload
records, and the `payload_padding_bytes` that follow it in the last payload
record are zero. The payload digest is:

```text
SHA256("REM-TAPE-INDEX-REPLICA-PAYLOAD-V1\0" || every complete fixed slot)
```

The canonical-map digest is the Section 7.3 canonical digest of the
structural rows.

**Header and footer frame.** The header and footer each occupy one complete
tape record. Their meaningful frame is the first `0x400` bytes; bytes from
`0x400` through the end of the record are zero. The CRC is CRC-64/XZ over
bytes `0x000..0x3F8`. The Kind column classifies each row by where its value
comes from, not by whether it differs between replicas.

| Offset | Size | Field | Kind |
| ---: | ---: | --- | --- |
| `0x000` | 8 | role magic | derived |
| `0x008` | 2 | schema version `1` | fixed |
| `0x00A` | 2 | role: header `1`, footer `2` | fixed |
| `0x00C` | 4 | flags: FINAL bit 0 required; other bits zero | fixed |
| `0x010` | 16 | tape UUID | common |
| `0x020` | 16 | nonzero final edition ID | common |
| `0x030` | 8 | nonzero final edition sequence | common |
| `0x038` | 2 | replica ordinal `1`, `2`, or `3` | local |
| `0x03A` | 2 | replica count `3` | fixed |
| `0x03C` | 4 | partition `0` | fixed |
| `0x040` | 4 | fixed block size | common |
| `0x044` | 4 | compression mode `0` (disabled) | fixed |
| `0x048` | 8 | covered-prefix tape-file count | common |
| `0x050` | 8 | total data ordinals | common |
| `0x058` | 8 | highest protected ordinal | common |
| `0x060` | 8 | `structural_row_count` | common |
| `0x068` | 8 | `object_row_count` | common |
| `0x070` | 8 | payload length | common |
| `0x078` | 8 | payload record count | common |
| `0x080` | 8 | replica record count | common |
| `0x088` | 8 | planned local tape-file number | local |
| `0x090` | 8 | planned local start LBA | local |
| `0x098` | 8 | forward footer block offset | common |
| `0x0A0` | 8 | planned terminal EOD LBA | common |
| `0x0A8` | 32 | domain-separated payload SHA-256 | common |
| `0x0C8` | 32 | canonical-map SHA-256 | common |
| `0x0E8` | 32 | edition digest | common |
| `0x108` | 32 | terminal-layout digest | common |
| `0x128` | 32 | replica descriptor digest | local |
| `0x148` | 160 | five planned 32-byte component tuples | common |
| `0x1E8` | 8 | reserved zero | fixed |
| `0x1F0` | 2 | writer-version byte length | derived |
| `0x1F2` | 2 | write-timestamp byte length | derived |
| `0x1F4` | 4 | reserved zero | fixed |
| `0x1F8` | 128 | printable ASCII writer version, then zero padding | common |
| `0x278` | 64 | valid RFC3339 timestamp, then zero padding | common |
| `0x2B8` | 32 | footer: complete header-record SHA-256; header: zero | local (footer); fixed (header) |
| `0x2D8` | 8 | footer local observed tape-file number; header: zero | local (footer); fixed (header) |
| `0x2E0` | 8 | footer local observed start LBA; header: zero | local (footer); fixed (header) |
| `0x2E8` | 8 | footer local observed record count; header: zero | local (footer); fixed (header) |
| `0x2F0` | 8 | footer observed footer LBA; header: zero | local (footer); fixed (header) |
| `0x2F8` | 8 | footer backward start delta; header: zero | local (footer); fixed (header) |
| `0x300` | 248 | reserved zero | fixed |
| `0x3F8` | 8 | CRC-64/XZ | local |

| Kind | Meaning | How it is checked |
| --- | --- | --- |
| common | Edition-common: a fact of the edition that A, B, and C share | Equal in every agreeing replica (Section 8.5); recomputed from, or bound by, the digests of this section |
| derived | Computed from edition-common values and the frame's role | Recomputed and compared |
| local | Replica-local: belongs to one replica | Compared with that replica's plan, its measured observation, or its recomputed digest |
| fixed | A format constant, a reserved zero, or a value set by the frame's role | Equal to the value the row states |

A replica-local value may coincide in all three terminal replicas of a healthy
tape: the observed record count (`0x2E8`) and the backward start delta
(`0x2F8`) are equal in A, B, and C by construction.

In a footer, the backward start delta (`0x2F8`) MUST equal the observed record
count (`0x2E8`) minus one, and the observed footer LBA (`0x2F0`) MUST equal the
observed start LBA (`0x2E0`) plus that delta. A Reader rejects a footer whose
values differ. The backward start delta lets a Reader positioned at the footer
find the header of the same replica; Section 10.6 requires it to name the
planned header start. In a header, all six footer
fields are zero.

The edition ID (`0x020`) is 16 bytes chosen by the Writer when it finalizes
the tape; it MUST NOT be all zero. The edition sequence (`0x030`) is a u64
chosen by the Writer; it MUST NOT be zero. A Reader rejects a zero value of
either. No Reader decision defined by this document depends on their values
beyond the zero check and equality: both are edition-common fields, so they
are equal in A, B and C (Section 8.5), and the edition ID is also equal in
both separation extents (Section 10.6), whose frame carries no edition
sequence.

Each magic is derived as in Section 5.2 from the tape UUID and the role's
label. The labels are exactly:

```text
header: "REM\0TIREP\x01H"
footer: "REM\0TIREP\x01F"
```

These magics classify structures; they do not authenticate them
(Section 16.1).

**Digest domains.** Every integer in these preimages is little-endian. The
common edition digest is SHA-256 over:

```text
"REM-TAPE-INDEX-EDITION-V1\0"
schema_version:u16
tape_uuid[16]
edition_id[16]
edition_sequence:u64
partition:u32
block_size:u32
compression_mode:u32=0
covered_prefix_tape_file_count:u64
total_data_ordinals:u64
highest_protected_ordinal:u64
structural_row_count:u64
object_row_count:u64
payload_len:u64
payload_record_count:u64
payload_sha256[32]
canonical_map_sha256[32]
writer_version_len:u64 || writer_version bytes
write_timestamp_len:u64 || write_timestamp bytes
```

The replica-local descriptor digest is SHA-256 over:

```text
"REM-TAPE-INDEX-REPLICA-DESCRIPTOR-V1\0"
edition_digest[32]
terminal_layout_digest[32]
replica_ordinal:u16
replica_count:u16=3
local 32-byte component tuple
footer_block_offset:u64
```

The header and footer repeat the same descriptor digest. A footer therefore
cannot bind another ordinal or location. The fields that replicas must share
are stated once, in Section 8.5.

### 10.5. Separation Extents

**Record geometry.** The nominal extent includes its header and footer. For
nominal extent bytes `E` and block size `B`:

```text
total_records    = ceil(E / B), required ≥ 2
footer_offset    = total_records − 1
interior_records = total_records − 2
actual_bytes     = total_records × B
```

One extent is a full header record, the zero-filled interior records, a full
footer record, and a trailing filemark. The frame records `E`, and Readers
derive the geometry from the recorded value; the default of Section 2.5 is a
Writer choice, not a validity condition. The on-media compression field is an
assertion that compression was disabled, not proof of it. `actual_bytes` is
not a recorded field, but the size-formula condition of Section 10.6 covers
it: its exact value MUST fit in u64. In the separation descriptor digest,
`total_records` is the recorded field at `0x058`.

**Header and footer frame.** The meaningful frame is `0x200` bytes and the
rest of the full record is zero. CRC-64/XZ covers bytes `0x000..0x1F8`.

| Offset | Size | Field |
| ---: | ---: | --- |
| `0x000` | 8 | role magic |
| `0x008` | 2 | schema version `1` |
| `0x00A` | 2 | role: header `1`, footer `2` |
| `0x00C` | 4 | flags zero |
| `0x010` | 16 | tape UUID |
| `0x020` | 16 | nonzero final edition ID shared with A, B, and C |
| `0x030` | 2 | separation ordinal `1` or `2` |
| `0x032` | 2 | separation count `2` |
| `0x034` | 4 | partition `0` |
| `0x038` | 4 | fixed block size |
| `0x03C` | 4 | compression mode `0` |
| `0x040` | 8 | planned local tape-file number |
| `0x048` | 8 | planned local start LBA |
| `0x050` | 8 | nominal total extent bytes |
| `0x058` | 8 | total record count |
| `0x060` | 8 | footer block offset |
| `0x068` | 8 | zero interior record count |
| `0x070` | 2 | predecessor replica ordinal |
| `0x072` | 2 | successor replica ordinal |
| `0x074` | 4 | fill kind `0` (all-zero interior) |
| `0x078` | 8 | predecessor tape-file number |
| `0x080` | 8 | successor tape-file number |
| `0x088` | 8 | predecessor start LBA |
| `0x090` | 8 | successor start LBA |
| `0x098` | 32 | terminal-layout digest |
| `0x0B8` | 32 | separation descriptor digest |
| `0x0D8` | 8 | planned terminal EOD LBA |
| `0x0E0` | 160 | five planned 32-byte component tuples |
| `0x180` | 32 | footer header-record SHA-256; header zero |
| `0x1A0` | 8 | footer local observed tape-file number; header zero |
| `0x1A8` | 8 | footer local observed start LBA; header zero |
| `0x1B0` | 8 | footer local observed record count; header zero |
| `0x1B8` | 8 | footer observed footer LBA; header zero |
| `0x1C0` | 8 | footer backward start delta; header zero |
| `0x1C8` | 48 | reserved zero |
| `0x1F8` | 8 | CRC-64/XZ |

Magic labels are exactly `"REM\0TISEP\x01H"` and `"REM\0TISEP\x01F"`,
HMAC-derived and truncated as for replicas (Section 10.4).

The separation descriptor digest is SHA-256 over, with every integer
little-endian:

```text
"REM-INDEX-SEPARATION-DESCRIPTOR-V1\0"
tape_uuid[16]
edition_id[16]
separation_ordinal:u16
separation_count:u16=2
partition:u32
block_size:u32
nominal_extent_bytes:u64
total_records:u64
local component tuple
predecessor component tuple
successor component tuple
terminal_layout_digest[32]
```

### 10.6. Replica Validity Conditions

A Scanner MUST NOT treat a terminal replica as locally eligible unless every
condition below holds. The conditions are a conjunction, and this document
does not fix the order in which a Reader checks them, except that a matching
role magic commits the tape file to its control type, so malformed control
never falls through to Object (Section 12.3).

- The header and footer role magics match the tape; matching magic with
  malformed content is a typed control failure, never an Object.
- Each frame's record length, CRC, schema version, role, flags, tape UUID,
  partition, block size, compression mode, ordinals and counts, reserved
  bytes, and padding validate against Section 10.4, and the block size is one
  of the terminal record sizes of Section 8.3.
- Every size formula of Section 10.4 evaluates without overflow, and the
  recorded geometry fields equal its results. As Section 2.4 defines it, a
  formula evaluates without overflow when its exact value fits in u64.
- The planned layout of the terminal suffix validates against Section 8.3, and
  its layout digest recomputes to the recorded value.
- The edition digest and the local descriptor digest recompute to the
  recorded values.
- The footer's backward delta names the same header start as the discovered
  planned layout; the header at that planned coordinate has a complete-record
  SHA-256 equal to the footer's recorded header hash, and header and footer
  carry the same common descriptor.
- The declared locations, the counts, and the trailing filemark agree with the
  device's measurements.
- Every fixed slot of the payload decodes, and the dense map, scope,
  deterministic CBOR, zero padding, exact map↔Object-row bijection, payload
  digest, and canonical-map digest validate.

The replica's pre-A structural map is locally eligible only when all of these
additional relationships hold:

- a `ParityMap` row, when present, is the final structural row;
- a final `ParityMap` is present if and only if at least one
  `ParitySidecar` row is present;
- when sidecars are present, `highest_protected_ordinal` equals
  `total_data_ordinals`, so finalization left no Object ordinal unprotected;
  both are the recorded fields, and Section 7.4 separately requires each to
  equal the value recomputed from the map;
- replica A's planned start LBA equals the end of the covered prefix,
  `Σ(block_count + 1)` over its structural rows (Section 3.2);
- `covered_prefix_tape_file_count`, `structural_row_count`, and replica A's
  planned tape-file number are equal.

A replica whose header breaks the covered-count relationship is not eligible,
and a Reader need not read its payload.

A separation extent is valid when its header and footer satisfy the
conditions in the first list above, read against the fields of Section 10.5
in place of those of Section 10.4, with two exceptions: the payload condition
does not apply, because an extent has no payload, and in the digest condition
only the separation descriptor digest is recomputed, because an extent's frame
carries no edition digest. Its edition ID MUST equal the replicas'. The
replicas here are those that validate and agree (Section 8.5). The
footer arithmetic of Section 10.4 applies to its footer fields (`0x1A8`,
`0x1B0`, `0x1B8`, `0x1C0`).

The validity of a separation extent does not affect whether a replica is
accepted for an inventory. A Scanner that reads only the replicas to return an
inventory need not read the separation extents. That rule concerns what the
extents contain. A record or tape file written into or after the suffix moves the tape's EOD past
the suffix's planned EOD, so that none of its footers supplies a layout (Section
8.4 step 1). Unless another footer supplies a layout whose replicas validate,
the Scanner then takes the walk of Section 8.4.1 although every replica is
intact. A filemark missing from the suffix does not
have this effect: the layout can still qualify, and a replica before the
missing filemark can still be accepted. A Verifier performing full
verification MUST check that every interior byte of both separation extents is
zero. An extent whose interior is not entirely zero is invalid for full
verification. A Verifier that finds a separation extent invalid MUST report it
and MUST NOT report the terminal suffix as complete.

## 11. Writer Obligations

### 11.1. Commit Discipline (per Tape File)

Every tape file — object, sidecar, bootstrap, ParityMap, terminal replica, or
separation extent — goes through one cycle:

```text
begin                      (at the durable boundary; dense numbering; one in flight)
→ write blocks             (any short write / EOM / completion-unknown ⇒ abandon)
→ trailing filemark        (immediate or synchronous; same failure rule;
                           EOM here ⇒ abandon — never commit)
→ synchronization proof    (the filemark's synchronous completion, or a
                           later shared barrier — see below; under
                           deferral, this step moves to the barrier for
                           every file it covers)
→ [object close only] emit queued sidecars (each its own write cycle)
→ off-tape commit record   (THE commit point, Section 3.4; at object close,
                           one record — or one durable transaction — covers
                           the object and every sidecar emitted at its close)
```

A synchronizing barrier (Section 3.5) covers every tape file written since
the previous synchronization proof, or since session start. It MUST complete
before the commit record of any file it covers takes effect. After a
completion-unknown, end-of-medium, or failed outcome at the barrier, every file
the barrier would have covered remains uncommitted. The per-file synchronous
filemark of the basic cycle is the one-file case of this rule.

A session that ends before a barrier commits nothing since the last completed
synchronization proof; resume proceeds per Section 14.

The object-close bundle is durable atomically: a crash before the bundle's
commit record leaves the object *and* the sidecars emitted at its close
beyond the durable boundary — a torn tail, physically superseded on resume
(Section 14) — so a committed prefix always satisfies the Section 11.2
bounded-restart rule.

Failure at any step MUST abandon the in-flight file. The
watermark `W` advances only after a sidecar's boundary commit.

Recommended practice for write sessions is described in the REM Implementation
and Operations Guide, under “Writing tapes: sessions, commit and resume”.

### 11.2. Epochs and Sidecars

Parity accumulates incrementally per data block (Section 6.3). At `S × k` data
blocks the epoch closes into a **pending** sidecar; no tape I/O occurs
mid-object. Pending sidecars are emitted as tape files when the current object
closes. A barrier may close a non-empty short epoch and emit its sidecar
without `FINAL_PARTIAL_EPOCH`. Finalization performs the same funnel with a
terminal reason: its short epoch, if any, carries `FINAL_PARTIAL_EPOCH`, after
which the Writer emits the final ParityMap when required and then the terminal
suffix. Implicit-zero positions never cause padding blocks to be written
(Section 6.4).

After each Object's close-and-emit bundle, unprotected ordinals MUST number
fewer than `S × k`. This is the version-1 bounded-restart rule: at most one
open epoch ever needs rebuilding (Section 14 step 2).

### 11.3. Finalization

Finalization is permitted only between objects; a mid-object request MUST be
refused. It closes the partial epoch, emits all remaining sidecars, writes one
final ParityMap when the resulting sidecar epoch directory is nonempty, fixes
one immutable snapshot including that structural row, and writes exactly
replica A, separation extent AB, replica B, separation extent BC, and replica
C, each followed by a filemark and persistence barrier. A finalized tape
accepts no further appends.

The first finalization transition permanently disables Object admission.
Failure enters `RecoveryRequired`; recovery may emit only missing tape files
of the terminal suffix, at proved positions. It cannot reopen the tape or
write a second terminal suffix (Section 3.4).

### 11.4. Session Preconditions

Drive hardware compression MUST be verified off before any parity write. The
effective value is recorded as bootstrap key 5. A parity bootstrap recording
`true` MUST be rejected by Readers and Writers alike (Section 16.3). A Writer
MUST treat hard end-of-medium mid-file as a commit failure (Section 11.1),
never as a signal to truncate an object.

## 12. Scanner Obligations

### 12.1. Inputs and Authority

An off-tape catalog is a cache. The mounted tape's identity comes from the
bootstrap or, when the bootstrap is unreadable, from the tape UUID supplied
under Section 8.4.1, and a finalized tape's selected terminal replica
supplies the authoritative inventory. If terminal selection yields no valid
replica, the Scanner returns the explicit BOT structural-recovery outcome and
walks from LBA 0.

### 12.2. The Walk

Per tape file: read the head block; measure the file's length by filemark
spacing (space to the next filemark; the file's block count is the position
delta minus one); a zero-block file or a missing trailing filemark is
structural damage; EOD at a file start ends the walk. The positions in this
measurement are device reports. A position that does not advance past the
filemark, or that goes backwards, is a device fault (`TapeIo`, Section 2.4),
and the walk ends with that error. A file whose trailing filemark is missing
before EOD is not a tape file of the map. The walk reports structural damage,
classifies the file by its head, when the head can be read, as a torn
candidate (an incomplete Object candidate when nothing else fits, Section
8.4.1), and ends there.

### 12.3. The Classification Ladder

The bootstrap at tape file 0 establishes the tape identity against which every
later classification is checked. When the bootstrap cannot be read, the
identity is the expected tape UUID supplied out of band (Sections 8.4 and
8.4.1). The items below are numbered for reference, not as an order of trial,
with two exceptions: the one for items 5 and 6 stated below, and the footer rule of
items 2 and 3, which applies only when no rung of items 1, 4 or 5 parses the
file. Items 1 to 6 recognise
kinds that are disjoint by magic; items 5 and 6 are two ways of recognising a
sidecar. Disjoint magics do not make the rungs exclusive: under the footer
rule, a file whose head can be read but which no rung of items 1, 4 or 5
parses can take its control type from its footer magic, as items 2 and 3
state.

Items 2 and 3 commit a tape file to its control type as soon as its magic
matches, as they state. For items 1, 4, 5 and 6, a rung *recognises* a tape
file only when the file passes every check of that rung, its measured block
count included; a file that fails one of them is not recognised. A rung that
parses a file but fails its measured count reports the failed classification.
A rung parses a file when the file passes every check of that rung except its
measured block count. For item 4 either route of that item may be the one
that parses. Item 1 checks that the file is tape file 0: a block that
carries the bootstrap magic at any other tape file is not parsed by item 1, and
the later rungs try the file.

At tape file 0 with supplied values, the first record is judged by
Section 8.4 before item 1.
When Section 8.4 treats that record as unreadable because of a medium error, or
because its content is not a usable bootstrap, the walk continues on the
supplied values and types tape file 0 by its measured length, without reading a
bootstrap from it. In that case a tape file 0 that measures exactly one block is
typed a bootstrap: the supplied values classify it and do not authenticate it,
and no value is taken from its bytes. In that case a tape file 0 that measures
more than one block cannot be a bootstrap (Sections 3.1 and 8.1), and the walk
ends with `FilemarkMapReconstruct` (Section 15), because it cannot produce a
valid map. In that case, of one block or more, neither the paragraph headed
“Unreadable head block”, below, nor the footer sentences of items 2 and 3, nor
the tail route of item 4 apply to tape file 0. A filemark or EOD where the first
record should be is a zero-block file, or EOD at a file start (Section 12.2); it
leaves no tape file 0 to type, and the walk ends with `FilemarkMapReconstruct`
for the same reason. A first record of the wrong length is refused, as Section
8.4 requires, and is not typed.

When a sidecar's
primary header parses, item 5 decides the sidecar rung for that file: if its
count fails, item 6 is not tried. Item 7 applies to a tape file that none of
items 1 to 6 recognises. A primary header parses when its block satisfies
Section 9.2 on its own: the magic, the tape UUID, both CRCs, every constraint
of the table, `copy_kind` 1 and the zero fill. A header that breaks one of
them, such as `sidecar_total_block_count`, does not parse, and item 6 then
tries the file.

1. **Bootstrap**: the fixed magic matches, the full frame parses, the frame's
   `block_size_bytes` equals the read size, the frame's `tape_uuid` equals the
   tape identity of Section 12.1, and the file measures exactly 1 block.
2. **TapeIndexReplica**: matching terminal-replica header magic commits the
   tape file to this control type. The header and measured block count are
   checked against the encoded component plan. A malformed frame or count
   mismatch is reported as a damaged terminal replica; it MUST NOT fall through
   to Object. When the head is unreadable, the file's last block is read, and a
   terminal footer whose magic matches establishes the same type, whether or not
   the footer parses. The footer takes the header's place in the check below: a
   footer that does not parse, or whose record count differs from the measured
   count, is reported as a damaged terminal replica. For
   items 2 and 3 the walk checks that the header parses and that the measured
   count equals the record count of the planned component. It does not compare
   the planned tape-file number or start position with the file's measured
   tape-file number or start position; Section 10.6 checks those when a
   replica is validated. A file that fails the check is reported as damage,
   not raised as an error, and is not an Object.
3. **IndexSeparationExtent**: matching separation-header magic commits the
   tape file to this control type under the same malformed-control and measured
   count rules. When the head is unreadable, a footer whose magic matches
   establishes the type, and is checked and reported as item 2 says. In items
   2 and 3 a footer whose magic matches
   establishes the type also when the head is readable, is not a header of
   that type, and no rung of items 1, 4 or 5 parses it. If the footer does not
   parse or the count disagrees, the file keeps its control type, damaged.
4. **ParityMap**: the first block carries the ParityMap magic and its header
   parses, and the measured block count equals the header's
   `parity_map_total_block_count`. A header parses when its block satisfies
   Section 10.1.3 on its own: the magic, the tape UUID, the block size, the CRC,
   every constraint of the table, and the locator counts, which agree with the
   values Section 10.1.2 derives from `payload_len` and the block size. The
   header may be either copy kind, 1 or 2. Away from tape file 0, when the first
   block is unreadable, or no rung of items 1 or 5 parses it and it is not a
   ParityMap header that parses, the walk reads the file's last block. If that
   block is a footer whose magic matches and that parses (Section 10.1.3), the
   footer locates the tail header copy at its `tail_copy_start_block`, and the
   file is a ParityMap if that tail header parses and agrees with the footer
   and the footer's `parity_map_total_block_count` equals the measured block
   count. If the footer parses and its total differs from the measured count,
   the failed classification is reported whether or not the tail header parses,
   and the file is not recognised. This is the copy fallback of Section 10.1.3, applied
   to the walk. This item reads no payload, and reads a footer only on that
   route. Sections 10.1.3 and 10.1.4 check the rest when the ParityMap is
   read, and a ParityMap that fails them is still the ParityMap of
   the walk. A file that neither route recognises is not recognised by this
   item, and the later rungs try it. It is classified as parity-closeout
   metadata written before the terminal suffix, not selected as terminal
   inventory authority.
5. **Sidecar (primary)**: the primary header parses, and the measured block
   count MUST equal the header's `sidecar_total_block_count`. On a mismatch the
   file is not recognised, and item 6 is not tried.
6. **Sidecar (footer/tail probe)**: the Scanner reads the file's last block.
   If it parses as a sidecar footer, the footer's total MUST equal the
   measured block count, and the Scanner verifies the tail header copy against
   the footer, field for field. On a mismatch the file is not recognised. The
   Scanner MAY classify from the footer fields alone if the tail copy is
   unreadable.
7. **Object, by elimination** — never by reading object content.

In items 2 through 6, a count-mismatch “hard error” is scoped to that rung and
that tape file's classification: the Scanner reports the failed
classification and continues the walk with the next tape file. It MUST NOT
abort the whole walk for that mismatch. Under items 2 and 3 the file keeps its
control type, damaged; under items 4, 5 and 6, as under item 1, the file is
not recognised, and item 7 applies.

**Unreadable head block:** the Scanner MUST NOT abort. It MUST measure the
file by filemark spacing, run the footer/tail sidecar probe, and otherwise
classify the file as an object candidate. The last block it reads for that
probe also serves the terminal footer probe of items 2 and 3. It also serves the
tail route of item 4, and a file that item 4 recognises by that route is a
ParityMap and not an object candidate.

*Rationale.* This is the no-circular-failure rule in action: the block needing
recovery may be the very block that would have classified the file.

After the walk, when the tape's final ParityMap validates and is marked
`is_final_directory`, the Scanner MUST perform a second pass that identifies
as a sidecar each Object candidate at a tape file a directory entry names
whose measured length equals the entry's `sidecar_total_block_count`. The new
entry holds the directory entry's `epoch_id` and protected range and the
measured block count. Identification uses only the tape file number and
measured length; it reads no sidecar block. The Section 13.1 digest and scope
checks confirm all identifications together; otherwise the Scanner retains
the unreconciled map. Each identification removes the file's blocks from the
Object data-ordinal sequence, so later Objects'
`first_parity_data_ordinal` values are recounted. Item 7's rule against reading
Object content applies to the second pass. The tail copy's hash is checked
before its metadata is used for rescue (Section 13.3 step 3).

### 12.4. Terminal Replica Validation

The Scanner first performs bounded terminal discovery from EOD (Section 8.4).
A candidate replica is locally eligible only when its header, streamed fixed-slot
payload, footer, local observations, planned layout, trailing filemark, CRCs,
and all digests validate (Section 10.6).

Survivor agreement, conflict, and degraded evidence are governed by
Section 8.5.

Map and Object rows that a Scanner emits before its terminal summary are
provisional: each belongs to one attempt at one replica, and only the attempt
the terminal summary selects is the inventory. An Inventory Consumer MUST
commit only the attempt named by the terminal summary and MUST discard
rejected or unselected attempts.

Recommended practice for streaming an inventory to an Inventory Consumer is
described in the REM Implementation and Operations Guide, under
“Reading tapes”.

If A, B, and C all fail, the Scanner returns an explicit
`BotStructuralRecoveryRequired` outcome and performs the Section 8.4.1 walk;
BOT Object classifications are likewise streamed before their terminal
summary. It MUST NOT convert missing terminal authority into an empty inventory
or infer the existence of a planned future component from A or B.

### 12.5. Epoch Isolation

Damage confined to one sidecar's metadata — any or all of its header copies,
its footer, its directory entry's health flags — MUST NOT degrade
classification, mapping, digest validation, or recovery of any other epoch. At
worst the damaged epoch becomes "metadata unavailable"
(`SidecarMetadataUnavailable`, scoped to that epoch by definition). The same
holds for damage to its directory entry. An entry that fails a precondition of
Section 13.3 is not available, and an available entry whose hash is damaged
contradicts the copies it would vouch for (Section 13.3 steps 1 and 3). Either way
only its own epoch is affected.

*Rationale.* Copy health is deliberately excluded from the canonical digest so
that *discovering* damage never invalidates the map (Section 7.3).

### 12.6. Terminal Completeness

A normal finalized tape has the exact terminal suffix of Section 8.3 and EOD
immediately after C's trailing filemark. Planned future tuples in an earlier
component never attest that the later component, its filemark, or its barrier
exists.

On a tape presented without host state, a validating C/B/A survivor attests
the complete terminal inventory carried in that replica. Missing or invalid
siblings make the result degraded; disagreement makes it a conflict. An
inventory is *degraded* when the Scanner found a replica of the planned
layout missing or invalid. A Scanner need not read every replica's payload
(Section 8.4 step 2), so a replica whose payload it did not read is not found
invalid for that reason; a Verifier's full check reports every replica. No valid
replica invokes the explicit BOT structural walk, whose result is recovery
evidence rather than a fabricated terminal edition. A structural artifact
after the exact terminal suffix is nonconformant and MUST NOT be admitted as
an Object. A structural artifact is any tape file, complete or torn, that
follows the last of the five files of the terminal suffix, whatever it
contains. The suffix is exact when the walk recognises, in order, replica,
separation extent, replica, separation extent and replica, none of them
damaged. The rule is one of scope: an artifact is not an Object of any
inventory. The walk may report it as it reports any file beyond the validated
scope, as an Object candidate of unknown identity or, when torn, as an
incomplete candidate (Section 8.4.1), so that an operator sees it.

## 13. Recoverer Obligations

### 13.1. Inputs

A validated, scoped map (Section 12); the bootstrap's scheme record, or the
scheme supplied out of band when the bootstrap is unreadable (Section 8.4.1);
and the failed addresses — `(tape_file_number, object_block_index)` pairs or
ordinals. A map produced by the Section 8.4.1 walk, after the second pass of
Section 12.3, is validated against the tape's final ParityMap when that
ParityMap validates and is marked `is_final_directory`, and the map's canonical
projection (Section 7.3) through the ParityMap's own entry hashes to its
`canonical_map_digest`. The validated scope and durable boundary are the first
`directory_scope_tape_file_count` tape files, ending with that ParityMap's
entry; `W` is `directory_scope_highest_protected_ordinal`. Validation checks
that `directory_scope_tape_file_count` is the ParityMap's tape file number plus
one, and recomputes `T` and `W` from that prefix and compares them with
`directory_scope_total_data_ordinals` and
`directory_scope_highest_protected_ordinal`, respectively, using the Writer
definitions in Section 10.1.1. A walked map whose projection does not hash to a
validated final ParityMap's `canonical_map_digest`, or whose prefix disagrees
with those scope fields, is not validated and gives the Recoverer no map, with
no fallback to the bootstrap's scope. Damage confined to one sidecar that
leaves its metadata unreadable or checksum-invalid cannot cause this refusal or
deny recovery of other epochs: identification does not depend on that metadata.
If none of that sidecar's header/index copies validates, only its epoch is
metadata-unavailable (Section 12.5). Without a validated final ParityMap, the
walked map gives no validated scope beyond the bootstrap's. The sidecar epoch
directory of the tape's final ParityMap is also an input whenever that
ParityMap validates and is marked `is_final_directory`.

### 13.2. Typed Refusals

The Recoverer MUST reject, as typed refusals distinct
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
   record copy health (both-usable / tail-lost / primary-lost). Such a footer
   is valid, and it decides, unless an available sidecar epoch directory
   entry (step 3) disagrees with it: a copy it contradicts is damaged, and the
   directory-assisted rescue of step 3 is not used. When an available entry
   disagrees with a valid footer, on the canonical metadata hash or on the
   tail copy's position, neither decides: a copy that either contradicts is
   not used. If no copy remains, the epoch is metadata-unavailable (step 4).
2. **Primary fallback.** If the footer is unreadable, unparseable, **or
   inconsistent with the map entry** — a footer that parses but contradicts
   the map is treated as an invalid footer, not as a hard stop — fall back to
   the primary header at block 0 (copy-kind and map-entry block-count
   cross-checks apply).
   - When a sidecar epoch directory entry is available (step 3), it decides
     as the footer would: a copy is used only if its `canonical_metadata_hash`
     equals the entry's. The primary is used if it does; otherwise step 3
     applies.
   - When no entry is available, a Recoverer whose primary copy validates MUST
     also read the tail copy at block `H + P`, where `H` is the primary's
     `sidecar_header_block_count` and `P = S × m`. The tail copy is valid when
     it satisfies Sections 9.2 to 9.5 on its own, its `copy_kind` is 2, and it
     passes the cross-checks of this step. A valid tail copy whose canonical
     metadata hash (Section 9.5) differs from the primary's leaves nothing to
     decide between them, and the epoch is metadata-unavailable. If the tail
     copy is unreadable or invalid, the primary is used. In steps 2 and 3 a
     copy is the whole header/index copy: the primary header names block 0 of
     the primary copy, and an index block whose CRC does not verify (Section
     9.3) makes the copy invalid.
3. **Directory-assisted tail rescue.** If the primary also fails and a sidecar
   epoch directory entry is available: locate the tail copy at block
   `sidecar_total_block_count − 1 − sidecar_header_block_count` using the
   entry's counts, and verify its `canonical_metadata_hash` against the entry.
   The same applies when the primary is valid but its hash disagrees with an
   available entry. For an entry that meets the preconditions below, that
   block is `H + P`, where `H` is the entry's `sidecar_header_block_count` and
   `P = S × m`. The preconditions below are checked first, and for an entry
   that fails them the location is not evaluated.
   An entry is available whenever the tape's final ParityMap validates and is
   marked `is_final_directory`, reached through a validated replica's
   structural rows or found by the Section 8.4.1 walk. That condition is
   necessary but not sufficient: the entry is available only if it also meets the
   preconditions that follow. A Recoverer MUST NOT
   place a read from a directory entry unless the entry agrees with the
   sidecar's map entry in tape file, epoch, protected range and block count.
   This applies to every map entry the rescue uses, on either route. The
   rescue also requires the entry's `sidecar_total_block_count` to equal
   `2H + P + 1`, with `H > 0`. An entry that fails either precondition is not
   available. These are
   preconditions of the rescue, not invariants of Section 10.1.5: an entry that
   fails them affects only its own epoch.
   On the walk route, the Scanner identifies each Object candidate at a tape
   file the directory entry names as that sidecar when its measured length
   equals the entry's `sidecar_total_block_count`, without reading its tail.
   The Section 13.1 digest check confirms the whole projection, including these
   identifications. The Recoverer checks the tail copy's `canonical_metadata_hash`
   against the directory entry before using the tail copy's metadata for rescue.
   The directory carries exactly the counts and hash needed to find and verify
   the tail copy without the footer — the case it exists for (Section 10.1.1).

   **Tail rescue from the terminal index.** If the footer and the primary
   copy have both failed, no final ParityMap validates (so the sidecar has no
   directory entry), and the sidecar's map entry comes from a validated
   terminal replica's structural rows, a Recoverer MUST try the tail copy
   that the map entry locates. With `total` the map entry's block count and
   `P = S × m`, the tail copy starts at block `H + P`, where
   `H = (total − 1 − P) / 2`. This rescue requires that `total − 1 − P` is
   even and that `H` is greater than zero. The tail copy is used only if it
   is valid on its own (step 2's sense), its recorded
   `sidecar_header_block_count` equals `H`, and its epoch and protected range
   agree with the map entry. If this rescue fails, the epoch is
   metadata-unavailable (step 4). A walked map does not qualify: it gains a
   validated scope only through a final ParityMap (Section 13.1), which this
   case lacks. When a final ParityMap validates but the sidecar's entry fails
   a precondition of the directory-assisted rescue, this rescue does not
   apply either, since the directory then contradicts the map entry. A final
   ParityMap that validates excludes this rescue whether or not it has an
   entry for the sidecar and whatever that entry says, including an entry
   that agrees with the map entry and fails only `total = 2H + P + 1`. A final
   ParityMap validates when it validates under Section 10.1 (at least one copy
   is valid, and the two agree when both are valid),
   its directory meets Section 10.1.5 and fits the scope of the map, and it is
   marked `is_final_directory`. A ParityMap that is `ParityMapParse` or
   `DirectoryInvalid` in every copy does not validate: the Recoverer continues
   without the directory, as when there is no ParityMap, and it does not
   report that error as the result of the recovery.

   *Rationale.* Nothing outside a copy found this way vouches for it. Its own
   CRCs and canonical metadata hash detect accidental damage, and every shard
   and rebuilt block is still checked against the index (Sections 13.4 and
   13.5).
4. Only when no header/index copy can be validated is the epoch
   **metadata-unavailable** — and only that epoch (Section 12.5). A copy that
   the footer or the directory contradicts, or that step 2 leaves undecided,
   is not validated. The Recoverer then reports `SidecarMetadataUnavailable`
   for the epoch, whatever each copy's own failure was. A rejection under
   Section 2.4 inside one copy is a failure of that copy and not of the
   request.

This is the **recovery-usable rule**: at least one valid header/index copy
plus CRC-passing needed shards ⇒ the epoch is usable. The acquired index
MUST then be pinned against the bootstrap's scheme record (`k`, `m`, `S`, block
size), or against the supplied scheme and block size when the bootstrap is
unreadable, and against the map entry's ordinal range; disagreement is
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
  the session. A record shorter or longer than one block is a read failure
  (Section 3.5). An address that the request names as failed is an erasure
  whether or not a read of it would succeed, and the Recoverer does not read
  it.
- **Implicit zero**: an ordinal ≥ `protected_ordinal_end_exclusive` — an
  all-zero shard supplied without tape I/O; not an erasure (Section 6.4).

### 13.5. Reconstruction and Release

Reconstruct per Section 6.5 from the first `k` trusted or implicit shards
(data shards first in index order, then parity). More than `m` erasures in
a stripe is unrecoverable — a typed result carrying the stripe and the
counts (`Unrecoverable{stripe, lost_count, limit}`). `lost_count` is the
number of erasures in the stripe, including the failed block, and `limit` is
`m`.

Every reconstructed data block MUST be verified against its sidecar data CRC
before release. A mismatch is an unrecoverable result, even though the matrix
algebra succeeded. It is typed distinguishably from parse failures and
refusals, for example as `Unrecoverable` with the stripe and counts.

*Rationale.* A mismatch means that some trusted input was wrong, and releasing
the output would convert detected damage into silent corruption.

### 13.6. Bulk Recovery (Informative)

Recommended practice for bulk recovery is described in the REM Implementation
and Operations Guide, under “Reading tapes”.

## 14. Resumer Obligations

A later session appends **after the last committed tape file** — not after
the last object, and not at the watermark.

1. Derive the committed prefix from the off-tape commit records
   (Section 3.4), dropping the tape's torn tail, if any, and compute `W` and
   `T` from it.
2. Enforce the version-1 bound: `T − W < S × k` (at most one open epoch).
   `W ≤ T`; committed sidecar ranges MUST be contiguous from zero through
   `W`; epoch ids MUST be consecutive; and the prefix's final object entry
   MUST end exactly at `T`. A violation is `ResumeAppend`. A committed prefix
   that records a terminal component or a final ParityMap describes a tape
   whose finalization has begun, and Section 3.4's transition to `Finalizing`
   permanently disables Object admission, so it is refused as
   `ResumeAppend` too.
3. Rebuild the open epoch by **re-reading ordinals `[W, T)` from the
   committed prefix on tape** — a boundary or short read where data is
   expected is fatal — recomputing per-block CRCs and re-accumulating
   parity. A read that finds a filemark, EOD or a record shorter or longer
   than one block where the committed prefix places a data block contradicts
   the commit record, and is `ResumeAppend`; a medium or transport failure is
   `TapeIo`. (Under the step-2 bound, `[W, T)` is the next open epoch range;
   a committed prefix never contains a complete unprotected epoch,
   because an object and the sidecars emitted at its close commit as one
   bundle — Section 11.1.)
4. **Position to the append point (`Σ(block_count + 1)` over the prefix)
   before writing anything.** The first block written lands exactly at the
   append point; a write issued anywhere else could land over committed data
   or short of the append point.

Anything physically on tape beyond the committed prefix is superseded by
the next append and MUST NOT be trusted for recovery.

Recommended practice for resuming a tape is described in the REM Implementation
and Operations Guide, under “Writing tapes: sessions, commit and resume”.

## 15. Errors

The error names below are normative for the test-vector manifests
(Section 17); surface syntax is not.

```text
NoBootstrapFound                bootstrap is absent or invalid
BootstrapParse                  bootstrap frame or payload violates Section 8
BootstrapPayloadTooLarge        framed BOT payload cannot fit the block
SidecarParse                    sidecar structure violates Section 9
SidecarMetadataUnavailable{epoch_id}   no header/index copy validated (Section 13.3)
ParityMapParse                  final ParityMap violates Section 10.1
DirectoryInvalid                well-formed sidecar epoch directory breaks an invariant of Section 10.1.5
TerminalIndexReplicaParse       replica framing or fixed-slot payload violates Section 8.3, 10.2–10.4, or 10.6
TerminalIndexSeparationParse    separation extent violates Section 8.3, 10.5, or 10.6
TerminalIndexReplicaConflict    independently valid survivors disagree
BotStructuralRecoveryRequired   no terminal replica validates; explicit BOT walk required
SchemeMismatch                  sidecar geometry disagrees with the bootstrap or supplied scheme
FilemarkMapDigestMismatch       replica structural projection digest mismatch, or a walked map's
                                projection or scope fields disagree with the final ParityMap
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

Some names cover cases stated elsewhere. `BootstrapParse` includes a
readable bootstrap that disagrees with supplied values (Section 8.4). It also
includes a no-parity bootstrap whose payload carries a parity scheme (Section
8.2).
`TerminalIndexReplicaParse` includes a replica map that describes a position
that does not fit (Section 7.2). `ResumeAppend` includes a commit record whose
values do not fit, and `TapeIo` a device-reported position that does not fit
or goes backwards (Section 2.4). `NoBootstrapFound` is a discovery outcome, as
the next paragraph states.

The bootstrap names depend on the level at which a Reader meets the block. A
parser given one block as the bootstrap reports a breach of Section 8 as
`BootstrapParse`, including a missing magic and a failed CRC, except that a
parity bootstrap recording drive compression is `DriveCompressionEnabled`.
Discovery over the candidate block sizes, without supplied values, reports
`NoBootstrapFound` when it finds no usable bootstrap at any candidate, and
`DriveCompressionEnabled` when a parity bootstrap records compression; a
record of another length only rules out that candidate. Discovery with only a
known block size is discovery over that one candidate, except that a record
of another length, or a bootstrap whose recorded block size differs from it,
is `BootstrapParse`. Discovery with supplied values follows Section 8.4.
`BootstrapPayloadTooLarge` is a Writer error.

Framing, CBOR, and type errors in a ParityMap, its directory included, are
`ParityMapParse`; a well-formed directory that breaks an invariant of
Section 10.1.5 is `DirectoryInvalid`. The name is normative as a category:
this document does not specify which of the two a Reader reports once it has
failed both ParityMap copies. The sentence applies to any pair of failed
copies, including two that fail for the same reason.

Refusals (Section 13.2), parse failures, and reconstruction failures MUST
remain distinguishable; I/O faults MUST remain distinct from format
violations. Section 16.2 describes the hazards of hostile input.

## 16. Security Considerations

### 16.1. No Authentication

HMAC-derived magics bind blocks to a tape UUID and a role; they are **not**
authentication — the UUID is public (it is in the bootstrap), so anyone
with the tape can forge consistent structures. CRCs and SHA-256 digests
detect corruption, not tampering. The trust anchors are external: an
off-tape catalog or audit chain, and content-level verification in the
payload format. A tape that self-validates proves self-consistency only.
The same holds for the terminal replica and separation magics, CRCs, and
digests of Section 10: they detect substitution and corruption and bind
self-consistent structures, but because the tape UUID is public, none of them
is a secret-key authenticity claim.

### 16.2. Hostile-Input Posture

All tape bytes are untrusted. The bounds below are normative. Every declared
count or length is validated against the measured physical extent. All
arithmetic on tape-derived values is checked. Reserved fields and declared
zero-fill MUST be verified zero. Misuse of reserved space is nonconformance,
and silent acceptance would foreclose 1.x extensions. The sole exception is
the bootstrap's trailing fill, which is excluded from acceptance decisions
(Section 8.1). CBOR decoding enforces the Section 5.3 subset. Writer-supplied
diagnostic text (bootstrap keys 3 and 4, and ParityMap keys 6 and 7) is
bounded in length. The same text is bounded in charset. A value violating
either bound is treated as absent (Sections 8.2 and 10.1.4).

*Rationale.* That text is bounded, and escaped wherever it is shown, because
those fields are the first human-readable text a diagnostic tool prints from
an unknown cartridge. The operator reading them is deciding whether the
cartridge is damaged or hostile, and the text is chosen by whoever wrote the
tape.

A hostile or damaged cartridge can also declare counts and lengths chosen to
exhaust a Reader's memory, or carry bytes chosen to crash a parser. Neither
changes what a conformant Reader concludes from a valid tape, but either can
stop a tool before it reaches a conclusion. Coverage-guided fuzzing of the
parsers and of the scan walk is REM-PARITY freeze criterion 3, recorded in
`specs/README.md`. Recommended practice for handling such media is described
in the REM Implementation and Operations Guide, under “Handling hostile
media”.

### 16.3. Compression Interaction

Parity correctness assumes block-to-media identity: the damage geometry model
(Section 3.3) is meaningful only if the Nth logical block occupies the Nth
physical block's worth of media. Hardware compression silently breaks that
correspondence while appearing to work. Hence the dual defense. The Writer
verifies compression off before writing (Section 11.4). The recorded
`drive_compression` flag (bootstrap key 5) makes a tape written with
compression enabled identify itself as nonconformant. A Reader MUST reject
such a tape. Both sentences concern a parity tape (Section 11.4): a no-parity
bootstrap may record compression.

### 16.4. Structure Leakage and Confidential Payloads

This format's structures are plaintext on the tape and content-blind at the
block layer, but they still reveal *shape*: the number of Objects, each
Object's block count, the write timeline (the timestamps and sequence numbers
that the bootstrap, the ParityMap and the terminal replicas record), and
per-block CRC-64 values of stored bytes. An unkeyed CRC of a stored block can
confirm a guessed block's content. Storing Objects in an encrypted
representation (for example, [REMENCRYPT] envelopes) prevents this, because it
makes every stored block — and therefore every CRC and parity computation — a
function of ciphertext.

Each terminal replica's fixed 256-byte Object-row slot is the designated
bounded surface for payload-binding recovery metadata. A plaintext REM-OBJECT
row exposes manifest location, manifest size, manifest chunk count, and
manifest digest; this is acceptable because plaintext REM-OBJECT objects are
not confidential against a tape reader. An encrypted REM-OBJECT row exposes
only recipient epoch ids, `metadata_frame_len`, and `key_frame_len`, all
already plaintext in the REM-ENCRYPT envelope stored in the same tape file.
The encrypted row MUST NOT carry plaintext manifest anchors
(`manifest_first_chunk_lba`, `manifest_size_bytes`, `manifest_chunk_count`, or
`manifest_sha256`). Those values describe confidential inner content and would
add leakage beyond the REM-ENCRYPT envelope.

## 17. Test Vectors

The vector archive `remanence-test-vectors.tar`, SHA-256
`77be73e780e9ff2c265c8357b6ba684b4c69800213820ae1331850f742b1d83d`, was pinned
by 1.0.0-draft.1 and is distributed unchanged with the published revision
1.0.0-draft.2 (2026-09-06). The terminal bytes of this revision are review-only
candidate vectors under
`fixtures/rem-parity-terminal-index-draft/`.

The candidate set contains minimal and multi-object inventories at each legal
record size: 256 KiB, 512 KiB, and 1 MiB. Every profile contains five byte
streams in the order of the terminal suffix: A, AB, B, BC, C. Filemarks and
EOD are recorded as structural expectations in `MANIFEST.tsv`, not encoded
into those files. Compact separation extents use a test profile with nominal
extent bytes `E = 3*B` (Section 10.5): one header, one zero interior record,
and one footer. The compact profile changes only the recorded nominal extent
and derived count; it does not change framing or validation rules. The default
one-GiB separation extents are exercised by the VTL matrix and remain part of
the supervised physical-media obligation. The multi-object candidate prefix
includes the one final pre-A ParityMap row produced by nonempty sidecar
closeout; that ParityMap is not part of the terminal suffix or a substitute
for A/B/C authority.

The generator is
`crates/remanence-parity/examples/generate_terminal_index_vectors.rs`. The
terminal verifier `tools/verify_terminal_index_vectors.py` re-derives HMAC
role magics, CRC-64/XZ, full-file SHA-256, header hashes, local observations,
record formulas, component ordering, dense file numbers, logical positions,
the zero interiors of the separation extents, and terminal EOD without calling
the Rust codec. Candidate bytes remain mutable until the specification
freezes; the terminal verifier's recorded derivation and incremental
implementation review are pre-freeze evidence rather than publication.

The `minimal-*` profiles above are terminal suffixes of a tape that holds only
the bootstrap. The set's `tape-images/` directory holds whole tapes. One
tape-image builder makes six images from inputs recorded beside them:
Appendix A.4's minimal tape, which also holds an Object, a sidecar and the
ParityMap; a tape with a short epoch (`S = 4`); a tape with two epochs; the
minimal tape with its replica B taken from a second edition, which differs
only in edition id and edition sequence; and two unfinalized tapes that stop in
a torn tail, one after a closed epoch and one with an epoch still open.
`tape-images/MANIFEST.tsv` pins the size and SHA-256 of every tape file, of
each torn tail and of the whole stream. The image bytes are regenerated from
the inputs, not stored.

The damage matrix, `tape-images/cases/`, holds 71 cases, and 69 of them are
pinned; `parity-map-and-sidecar` and `e1-07` are not. Each case gives the
outcome it expects under this document: the inventory a Scanner reports, the
result of the walk, each failed address's recovery or error, or what a
Verifier reports. The matrix grew in five groups.

Of the first 25 cases, all but one make stated records of an image
unreadable or remove a filemark; the other reads the second-edition image
undamaged. One of them (`replicas-all`) expects a Scanner to run the walk
that Section 8.4.1 requires it to offer. Two (`bootstrap-hinted` and
`bootstrap-wrong-scheme`) expect the discovery with supplied values that
Section 8.4 requires. Their outcomes were written from this document before
any reader was run on them, and 24 of the 25 are pinned. The outcome pinned for
`filemark-prefix` follows Section 12.3's rule that a rung which fails its
count does not recognise the file. `parity-map-and-sidecar` is informative
because this document did not decide part of it when its outcome was
written; Section 13.3's tail rescue from the map entry now decides it, and
the case is to be pinned to that outcome.
The outcomes of the burst cases (`burst-m`, `burst-m-plus-one`,
`short-epoch-burst` and `short-epoch-recoverable`) follow from Sections 3.3,
9.1, 13.4 and 13.5; Appendix B.2 illustrates them and is informative. Their
pinned loss counts are Section 13.5's `lost_count`: the number of erasures in
the stripe, including the failed block.

The other 46 cases were added later, in four groups. Fifteen (`e1-01` to
`e1-15`) judge the first record with supplied values under Section 8.4. Ten
(`e2-01` to `e2-04` and `e2-08` to `e2-13`) cover wrong-length records, the
acquisition of the sidecar index and the walk (Sections 3.5, 8.4.1, 13.3),
and seven (`e3-01` to `e3-07`) cover the walk after damage to or beside the
terminal suffix (Sections 8.4, 8.4.1, 12.3, 12.6). Fourteen (`s6-01` to
`s6-14`) cover how Section 12.3 types a walked tape file: tape file 0 whose
record is unreadable or does not parse, a bootstrap's record at a later tape
file, a file that runs into the next when a filemark is lost, a control file
whose footer matches and does not parse, and a ParityMap whose payload, footer
or first block is damaged or whose first block cannot be read. An overlay,
`tape-images/expected-e4.json`, gives the outcome of the tail rescue of
Section 13.3 for `parity-map-and-sidecar`, and what a Verifier reports for 14
cases.

The resume vectors, `tape-images/resume/`, give a committed prefix over an
image as Section 7.1 entries, with `W` and `T`. Two resumes of unfinalized
images are accepted, one at `W = T` and one at `W < T`, and the resumed tapes
are pinned; the second one's sidecar equals the one an uninterrupted session
writes. Six prefixes, four over the unfinalized images and two over the
two-epoch image, break a Section 14 rule and are refused. Three more, `e2-05`
to `e2-07`, are refused before positioning or writing: a wrong-length record
at the re-read of step 3 and a committed prefix that records a final ParityMap
give `ResumeAppend`, and a medium failure at the re-read gives `TapeIo`. The
commit-record
cases of Section 3.4 depend on an implementation-defined record format and are
not portable.

The negative vectors, `tape-images/negatives/`, hold 58 cases and a supplement
of 48 variants. They cover the sidecar, the ParityMap, the bootstrap, the
terminal replicas and separation extents, and an overflow in each size and
location formula that a vector can reach. Each gives the mutation at the level
of this document's tables and the checksums repaired so that the mutation
reaches its rule, or the formula's inputs where only a test of the formula
itself reaches it, or the reason no vector can exist. Many cases break more
than one rule; each variant breaks one, and the supplement records 14 entries
that cannot be isolated. Each gives the Section 15 error this document
requires, the set of errors it permits, a required rejection with no named
error, or an informative outcome. The `section15` column of `MUTATIONS.tsv`
gives the same for the earlier terminal mutations.

The second implementation, `tools/rem_parity_second_implementation.py`, was
written from this document by an author who did not read the reference
implementation. It imports two things. The first is layout-independent
arithmetic: the Galois-field, Reed–Solomon, CRC-64/XZ and deterministic-CBOR
functions of `tools/rem_parity_rederive.py`, which was written alongside the
generation-1 reference and which the second implementation checked against
this document's worked values. The second is a plaintext REM-OBJECT builder
written from REM-OBJECT. It re-derives the
images, the terminal profiles, and the maximum, Object-row extension and
streaming artifacts from their recorded inputs, among them the bootstraps,
Objects, sidecars with their Reed–Solomon parity, and ParityMaps, and it
re-derives the resumed tapes. It decided every damage case, every portable
resume case and every negative case and variant before seeing the expected
outcome; one variant was corrected afterwards and not decided again. Its
records, under `second-implementation/`, show no disagreement with the
expected outcomes except on questions this document leaves open, and list
each place where it found this document silent or ambiguous. One expected
negative outcome was corrected by citation after review, and its decision
agrees with the correction.

The candidate set holds the candidate evidence for freeze criterion 2.
Appendix D item TT-2 lists what remains open before the criterion is met.

How candidate vectors are handled until this document is frozen, and what the
negative candidates must cover by then, are recorded in the release record,
`specs/README.md`, with the freeze criteria (Section 18).

## 18. Freeze Criteria and Status

The criteria that gate the freeze of this specification are kept, under their
numbers 1 to 6, in the project's release record: the section “How a revision
is frozen” of `specs/README.md`
(<https://github.com/archivetechie/remanence/blob/main/specs/README.md#how-a-revision-is-frozen>).
They are not all satisfied in this review draft; Appendix D lists the items
that remain open.
After freeze, revisions are governed by the change policy in the Status of This
Document section: errata and conforming minor revisions are permitted, and
anything that would invalidate an existing tape, change the meaning of
anything already written, or leave an earlier Reader unable to identify and
cleanly refuse a newer one is a new major version.

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

- One epoch protects `S × k = 65,536` data ordinals = 16 GiB of object data.
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

A smallest-useful finalized generation-2 tape has the bootstrap followed by
its Object/ParitySidecar prefix, the one final ParityMap that a nonempty
sidecar epoch directory requires (Section 10.1.1), and the exact terminal
suffix:

```text
file 0   Bootstrap
file 1   Object
file 2   ParitySidecar
file 3   ParityMap
file 4   TapeIndexReplica A
file 5   IndexSeparationExtent AB
file 6   TapeIndexReplica B
file 7   IndexSeparationExtent BC
file 8   TapeIndexReplica C
EOD
```

Each replica carries the same four structural rows, one for each of files 0
to 3 (Sections 10.2 and 10.6), and one Object recovery row. A Scanner reads
the replicas from EOD and exposes the inventory of one that validates and
agrees with every other valid replica (Section 8.5).
If all three are invalid it reports terminal authority unavailable and
performs the explicit BOT structural walk; it never treats the tape as empty.

## Appendix B. Design Rationale (Informative)

This appendix records the reasoning behind non-obvious decisions, so future
revisions do not silently reverse them.

### B.1. Parity Lives in Separate Tape Files

No parity byte ever appears inside an object tape file. Objects stay
contiguous and clean — a tar-based payload remains extractable with `mt` +
`tar` alone — and the parity geometry stays independent of object
boundaries. The cost, sidecars consuming their own tape files and
filemarks, is small at archival object sizes.

### B.2. The Interleave (Data and Parity)

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
at LBA `S × k + 1 + H + j·S + s` (the `+1` is the terminating filemark),
spaced `S` apart and phase-shifted from the data lattice by `1 + H`.
Destroying a stripe requires erasing `m + 1` of its shards; the shortest
contiguous span covering its data shard and `m` parity shards is

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

### B.3. Implicit Zeros Instead of Padding Blocks

A short epoch is closed by *declaring* the missing logical positions
all-zero rather than writing padding blocks. Tape capacity is never spent
on filler; the sidecar's `real_data_shard_count` tells a Reader which
positions are implicit; and the parity arithmetic is unaffected because
all-zero shards contribute nothing to any accumulator.

### B.4. Derived Magics

Sidecar, ParityMap, terminal-replica and separation-extent magics are
HMAC(tape_uuid, role label) so that a block can be attributed to *this tape*
and *this role* without any further context — stale blocks from a recycled
tape, or blocks from another tape in a mixed pile, fail the magic check
immediately. The bootstrap's magic must stay fixed: it is the entry point read
before the UUID is known. Derived magics are identity, not security
(Section 16.1).

### B.5. The Bootstrap Endianness Mix Is Frozen

The bootstrap header mixes big-endian integers with a little-endian length
and CRCs (Section 8.1). It looks like an accident; it is recorded here
precisely because "normalizing" it would break every existing tape. All
other structures are uniformly little-endian.

### B.6. The Canonical Digest Excludes Positions, Hashes, and Health

Three exclusion classes keep the digest non-circular and stable
(Section 7.3): physical positions would change as control files are
emitted; content hashes of control files would make the digest depend on
bytes whose own validation depends on the digest; and copy-health flags
would mutate the digest at *read* time, invalidating the map by the act of
discovering damage. The digest covers structure only — which is exactly
what recovery needs to be fenced by.

### B.7. Three Complete Replicas Are Separated Physically

Each terminal replica is independently usable: full header, streamed body,
local footer, and trailing filemark. The two separation extents are physical
separation, not additional index copies. Three complete replicas tolerate the
loss of either end and one middle region without any geometric placement rule.
This holds while the tape's EOD is no later than the planned EOD; when it is
later, the walk of Section 8.4.1 is the recovery.
Requiring surviving editions to agree prevents ordinal preference from hiding
a split authority.

### B.8. Checkpoint Authority Stays off Tape

Open-tape commit state lives in durable off-tape commit records (Section 3.4).
Ordinary checkpoint barriers close any pending parity epoch, prove the covered
files durable, and advance those records; they do not append a bootstrap or an
index. On restart, the Writer reconciles the measured physical prefix against
that host authority before it may append. Bare-tape inventory becomes complete
only when finalization writes the three identical terminal replicas. If none
survives, Section 8.4.1 offers structural recovery evidence without inventing
commit authority. This keeps open-tape checkpoint frequency independent of the
number or placement of on-tape index copies.

### B.9. Content-Blind Classification

The Scanner classifies object files by elimination, never by reading object
bytes (Section 12.3). This is what payload independence means physically: a
tape of foreign objects is mappable by any conformant implementation, an
unreadable object head block cannot derail the walk, and object formats
need no registered magics with this layer.

### B.10. At Most One Open Epoch

The bounded-restart rule (Section 11.2) caps unprotected ordinals below
`S × k` at every object boundary, so a Resumer rebuilds at most one open epoch
by re-reading at most `S × k − 1` blocks (16 GiB at the default geometry).
Without it, resume cost would grow with the number of epochs left open —
unbounded re-read of a tape that was supposedly fine.

### B.11. Sidecar Metadata Is Replicated Head and Tail, With a Locator

The header/index copy is written before the parity shards *and* after them,
with a footer locator at the very end. Contiguous damage at either end of the
sidecar file leaves a survivable copy at the other; the footer makes the tail
copy findable without trusting block arithmetic; and the sidecar epoch
directory makes it findable even with the footer gone (Section 13.3). The
canonical metadata hash is copy-independent, so any surviving copy is
verifiable against any directory entry. When the footer, the primary copy and
the final ParityMap are all lost, the terminal index's record of the sidecar
still locates the tail copy (Section 13.3), which is then trusted on its own
checksums.

### B.12. The Reference Off-Tape Journals Are Not a Media Format

The reference implementation keeps its commit records in two append-only host
journals per tape, which are not a media format: neither is recorded on tape,
and neither changes any REM-PARITY media byte. The implementation's
documentation describes them
(<https://github.com/archivetechie/remanence/blob/main/docs/reference-tape-layout.md#on-disk-durable-records-and-rebuildable-state>).

## Appendix C. Revision History (Informative)

Entries are newest first. Each carries: date · version · kind
(erratum / minor / major / draft milestone) · what changed and its effect on
conformance. Milestones that predate the first published revision are marked
`[draft]`; they were reached in public working drafts, not in published
revisions of this specification, and the change policy of the Status section
governs only the revisions that follow the first published one.

Each entry keeps the appendix lettering of the revision it describes: entries
before draft.5 use C for the closure record, D for the revision history, and E
for the open items. This lineage's "1.0.0-draft.2" entry (2026-08-02) is not
the published revision 1.0.0-draft.2 (2026-09-06, SHA-256
`1e4ad796bd77291f731d8d2611231984ba778f3650c1597f80eebdc7ba359fd2`), which is
an errata revision of draft.1.

- **2026-09-06 — 1.0.0-draft.5 — replacement review draft.** States that the
  concept DOI remains reserved pending the first deposit, corrects the
  generation-number references in the shared status policy, and identifies
  the criterion-5 clean-room exercise as technical independence by an AI
  system rather than organizational independence.

  The same revision makes the text complete and correct for generation 2. It
  changes no tape byte and no behaviour of the reference implementation; where
  the earlier text described Reader behaviour differently from the reference
  implementation (the discovery order of Section 8.4 and the agreement set of
  Section 8.5), the text now states what the reference implementation does. It
  restores, adapted to generation 2, the ParityMap text that draft.4 dropped:
  Section 10.1 now gives the purpose of the sidecar epoch directory, the copy
  layout, the payload map with its key 6 and 7 obligations, the Reader's
  locator and agreement rules, and the SidecarEpochDirectory with separate
  Writer rules and decoder invariants, and Section 15 gains
  `DirectoryInvalid`. It moves the exact terminal bytes into the document: the
  planned layout and layout digest into Section 8.3, and the structural rows,
  the Object recovery rows (formerly a subsection of Section 8.2), the replica
  frame with a Kind column, the separation extents, and the replica validity
  conditions into Sections 10.2 through 10.6, which renumbers Section 10. The
  replica-agreement rule is stated once, in Section 8.5, over the
  edition-common fields of the replica frame, and the discovery steps of
  Section 8.4 consider every valid replica before accepting one. It makes
  these corrections:

  - It corrects the sidecar `schema_version` in Section 9.2 from 1 to 2.
  - It makes Bootstrap keys 20, 21, and 30 reserved.
  - It scopes the Section 8.1.1 registry to `schema_major` 2.
  - It replaces the deposit claim of Section 17 with the archive digest.
  - It removes the Status sentence that called every Section 18 criterion
    satisfied.
  - It names the slot counts `structural_row_count` and `object_row_count`.

  It also writes down rules the reference implementation already enforces but
  no earlier revision stated:

  - the record-count relations of Section 8.3;
  - the ParityMap copy agreement, the u32 bound on directory flags and the
    Writer rule that the sidecar epoch directory is marked final, in
    Section 10.1;
  - the structural-slot container and the encoded-length bounds of every fixed
    slot, structural and Object, in Section 10.2;
  - the footer arithmetic and edition-identity rules in Section 10.4;
  - the separation-extent validity rules in Section 10.6.

  The draft.1 closed-item snapshot, formerly Appendix C, described the
  checkpoint-bootstrap/parity-map design and does not establish conformance to
  the generation-2 replacement; that appendix is removed, and the revision
  history and the open items become Appendices C and D. No tape byte changed.

  Section 1.5 is added. It states which rules belong in this document and
  points to the REM Implementation and Operations Guide for the rest. A rule
  about what an implementation does stays only if ignoring it would do one of
  three things. It would produce bytes another conformant reader cannot read
  or would misread. It would let two conformant readers reach different
  conclusions about what a medium or an object contains. Or it would let a
  tool claim something the bytes do not support. The same section appears,
  word for word, as the last subsection of Section 1 of REM-OBJECT,
  REM-ENCRYPT and REM-PARITY.

  The rules that Section 1.5 places outside this document have now left it.
  Most went to the REM Implementation and Operations Guide, whose chapters on
  writing tapes, finalizing a tape and recovering from a crash, reading tapes,
  and capacity admission this change writes. Those chapters hold what a commit
  record should contain and how several records share commit authority,
  barrier batching, staged records, writer poisoning, position tracking, the
  compression check, resuming a tape, the finalization lifecycle as a tool runs
  it, the BOT walk's notice, progress, abort and retry budget, the selection
  order among agreeing replicas, the streaming inventory interface, when
  recovery refusals are made, bulk-recovery planning, and admission refusal.
  Other chapters of the Guide hold the allocation, panic, escaping and fuzzing
  practice of Sections 5.3, 10.2, 10.6, 15 and 16.2, reporting every
  nonconformity, exposing typed errors, the advice to encrypt confidential
  payloads, and the Writer rules for the descriptive fields: bootstrap keys 3
  and 4, ParityMap keys 6 and 7, and a random edition identity. The
  description of the reference implementation's journals, formerly the body of
  Appendix B.12, went to that implementation's documentation. The freeze
  criteria of Section 18, under their existing numbers, the rule on handling
  candidate vectors and the negative-vector coverage list of Section 17 went to
  the “How a revision is frozen” section of `specs/README.md`.

  Some rules stay in a changed form. Section 3.4 now says that a Writer does
  not report a tape as `Finalized` while fewer than three complete replicas
  exist, instead of prescribing that it retain `RecoveryRequired`; the
  `FinalizedDegraded` transition is unchanged. Section 8.2 now lists key 3 as
  optional, as key 4 already was; readers still tolerate its absence, and key 5
  stays required. The advice to payload formats in Section 4.3 and the advice
  on encryption in Section 16.4 are now informative text. Section 13.2 keeps
  its refusals and no longer says when they are made. Section 14 states step 4
  as its outcome, and its step 5 has left. Where a paragraph or more left, the
  section points to the Guide.

  No byte of the format changed, and no valid tape or vector changed. This
  document no longer requires a conformant tool to survive hostile input, to
  report every nonconformity, to report progress to an operator, to record
  which software wrote a bootstrap, or to refuse an Object that would leave
  too little room to finalize; the Guide recommends each.

  The readability pass then changed the text as follows. No byte of the format
  changed, and no valid tape or vector changed.

  - Sections 10.5, 12.3, 13.2 and 18 are retitled “Separation Extents”,
    “The Classification Ladder”, “Typed Refusals” and
    “Freeze Criteria and Status”, so that each title describes what its
    section now holds. Appendix B's headings and the qualifiers of
    Sections 8.1, 9.2, 9.6, 10.1.3 and 11.1 take title case.
  - Section 2.2 defines two roles: the Reader, any role that reads a tape,
    including the Writer when it reads back what it has written; and the
    Inventory Consumer, which receives a Scanner's inventory. A requirement
    that bound “readers”, “decoders” or a “Consumer” now names one of these
    roles, or the single role it binds, such as the Recoverer in Section 3.4.
    The rules of Sections 5.3, 7.4 and 10.1.5 keep the condition under which
    they apply.
  - Section 2.3 defines the terminal suffix, the terminal replica, the replica
    envelope and the separation extent, and points to the sidecar epoch
    directory. The document uses these terms where it said “gap”,
    “typed extent”, “member”, “terminal triple”, “five-file suffix”,
    “pre-tail” or a bare “envelope”, and names the tape-file kinds by them.
  - Section 2.3 states that the capitalised word “Object” is the defined term
    and that attributive uses are lower case. In running text the bootstrap is
    “the bootstrap”, and “BOT” stays only where the position is the point. The
    producing role is written “Writer” throughout.
  - The symbol `E` now means only a separation extent's nominal bytes
    (Sections 10.5 and 17). Section 3.3 and Appendices A.1 and B.2 write an
    epoch's size as `S × k`.
  - Section 2.1 declares that a paragraph opening with *Rationale.* is
    informative, and that the steps of a numbered list are taken in the order
    given unless the text says otherwise. Section 12.3 says that its items are
    numbered for reference, not as an order of trial. An explanation of why a
    rule exists now stands in a *Rationale.* paragraph.
  - Section 1.3 states how this document cites its companions, and
    Section 10.3 cites REM-ENCRYPT §5.2 and REM-OBJECT §4.5.1 in that form.
  - The Status section gives this document's version axes as a table, and
    limits “never reused for different bytes” to the version string of a
    deposited revision. The legacy terms for generation 2, “replacement” and
    Section 5.2's “retained codec”, are gone from the Status section,
    Sections 2.5, 5.2, 8, 8.1, 8.1.1, 8.2 and 8.4, and Appendix D.
  - Sections 1.2 and 3.1 say that the labels in their diagrams abbreviate the
    terms of Section 2.3, and Section 1.2 and Appendix A.4 describe replica
    selection by its outcome, since the order of trial is the Guide's.
  - Errata in the tables and the ladder: Section 12.3 item 1 checks the
    frame's `tape_uuid`, which the payload does not carry, and the ladder now
    says where the tape identity comes from; the Section 8.1.1 registry names
    REM-PARITY 1.0 as the revision that defines value 3, and reserves values 0
    to 2 permanently: they are never assigned under `schema_major` 2; and
    Section 8.2 lists key 3 as optional in its Presence cell, as key 4 is,
    while the text below the table keeps the Reader's obligation.
  - Errata in the text: the Abstract places the ParityMap before the terminal
    suffix; Section 9.6 cites Section 12.3 item 6, the sidecar footer probe,
    which the renumbering in draft.4 had left as item 4; Section 3.4 no longer
    refers to “required contents” that moved to the Guide; Section 4.2 no
    longer names a function of the reference implementation; Section 16.4 no
    longer speaks of several bootstrap timestamps; and the paragraph above on
    Section 1.5 says where that section stands in each document.
  - The draft.1 entry's audit narrative is cut to the readings of the text it
    concluded, one bullet each; how the implementation was fixed and tested is
    left to the implementation's own record.
  - Sentences that stated more than one requirement, or a requirement and an
    aside, were split, among them Section 3.3's statement of where parity
    shards live and Section 16.2's bounds on diagnostic text. Where a split or
    rewritten requirement binds a role, it now names that role.

  The vector step then changed the text as follows. No byte of the format,
  valid tape or vector changed.

  - Erratum, Appendix A.4. The minimal tape now includes, as tape file 3, the
    one final ParityMap that Sections 10.1.1 and 10.6 require when final
    parity closeout has a nonempty sidecar epoch directory, and its replicas
    carry four structural rows. The appendix is informative; no rule changed.
  - The Status section, Section 17 and Appendix D item TT-2 now say that the
    candidate set does not yet meet freeze criterion 2, and what the
    independent verifier does not yet re-derive. TT-2 also says that no
    vector yet covers append resume, and how the pinned archive's
    generation-1 cases are covered.

  Decisions on questions the review raised then changed the text as follows.

  - Sections 13.1 and 13.3 now make the sidecar epoch directory available
    whenever the tape's final ParityMap validates and is marked final, through
    a validated replica's structural rows or the Section 8.4.1 walk. Section
    10.1.1 defines the Writer's four authority fields: the canonical digest
    covers tape files 0 through the ParityMap's own entry, the scope count is
    its tape file number plus one, and the total-data and highest-protected
    ordinal fields are that prefix's `T` and `W` at finalization. Section 13.1
    validates the reconciled walked projection, scope count and recomputed `T`
    and `W` against those fields, establishing its scope and durable boundary. A
    mismatch with a validated final ParityMap gives no map, with no fallback
    to the bootstrap's scope; only when no final ParityMap validates does the
    bootstrap's scope remain. Sections 12.3 and 13.3 require walk identification
    by directory tape file and measured length, confirmed by the Section 13.1
    digest and scope checks. Identification reads no sidecar metadata, so
    unreadable or checksum-invalid metadata cannot prevent identification of
    other epochs. The tail hash is checked before the tail's metadata is used
    for rescue, so unvalidated tail metadata cannot guide recovery.
    Section 13.3's Recoverer prohibition covers every sidecar map entry the
    rescue uses on both routes: agreement is checked before placing a
    directory-assisted read. Section 15 widens `FilemarkMapDigestMismatch` to
    cover a walked map whose projection or scope fields disagree with the final
    ParityMap, retaining the replica structural projection case. The Recoverer
    rule settles what Section 13.3 left undecided. The Writer definitions, the
    Scanner's second pass and the Recoverer rule are a minor change of the draft.
  - Section 8.4 now points to Section 8.5 when footers propose different
    planned layouts. The pointer is informative and changes no requirement.
  - Section 8.4 now permits a Scanner to discover terminal replicas when the
    bootstrap is unreadable and the expected tape UUID, block size and parity
    scheme are supplied out of band. A readable bootstrap that disagrees
    with the supplied values is refused, not overridden. Section 8.4.1 now
    names the parity scheme (`k`, `m`, `S`, or no parity) as a third mandatory
    hint for the BOT walk. The Reader permission and the third mandatory
    hint are a minor change of the draft.
  - Sections 5.2, 9.2 and 12.1 now name the supplied tape UUID as the source
    of tape identity when the bootstrap is unreadable, and Section 12.3's
    pointer also cites Section 8.4. Sections 6.6, 13.1 and 13.3 now name the
    supplied scheme for that case, and Section 15's `SchemeMismatch`
    description includes it. These changes carry the hint rule through the
    identity and scheme checks.
  - Section 10.3 now points to REM-ENCRYPT §5.3 for distinct epoch ids in
    every key frame and the Sealer's nonzero rule. The Object recovery row
    still requires both. The pointer adds no requirement.
  - Appendix D gains item TT-7: which bootstraps count as unreadable is to be
    decided before freeze.

  No byte of the format changed, and no valid tape or vector changed.

  The fixture step then changed the text as follows. No byte of the format and
  no valid tape changed; the candidate set gained the vectors Section 17
  describes.

  - Section 17 now describes the tape images, the damage matrix, the resume
    vectors, the negative vectors and the second implementation, and no longer
    says that they do not exist. It removes the sentence that the verifier does
    not yet re-derive bootstrap, sidecar or ParityMap bytes, or Reed–Solomon
    parity, and TT-2 removes the sentence that no vector yet covers append
    resume. It calls the verifier of the terminal bytes the terminal verifier.
    The Status section says that the evidence for freeze criterion 2 is
    gathered and that the criterion remains open until the items of TT-2 close.
    Appendix D item TT-2 records the evidence and lists what remains open: the
    values the second implementation does not yet re-derive, and the formulas,
    questions and readings the text has yet to settle.
  - Wording follow-ups of the vector step change no requirement. Section 10.3
    says which rules key 22's recipient epoch ids obey; Section 14 step 1 says
    "the tape's torn tail"; Section 17 records the reading of `lost_count`
    behind the burst cases; the Status section and TT-1 describe the terminal
    verifier as Section 17 does; and paragraphs left with ragged wraps by
    earlier edits are rewrapped.

  Rulings on the questions the fixtures raised, and readings of the text the
  fixtures showed to be open, then changed the text as follows. No byte of the
  format changed. Where a Reader's outcome changes, the change is stated. The
  rulings, and the new requirements of Sections 2.2, 2.4, 3.5, 7.2, 8.4, 9.6,
  10.1.3, 10.1.4, 12.3 and 14, are a minor change of the draft.

  - Section 12.3 defines when a rung recognises a tape file. Items 2 and 3
    commit a file to its control type when the magic matches; items 1, 4, 5
    and 6 recognise a file only when it passes every check, its count
    included; a file that parses at such a rung but fails one of its checks is
    not recognised, and a count mismatch is reported as damage. A primary
    sidecar header that parses decides the sidecar rung, so item 6 is not
    tried after item 5 fails. With the head unreadable, a terminal footer now
    establishes the control type, where the text had said it may.
  - Sections 9.1 and 13.3 say which copy of a sidecar's index a Recoverer uses
    when the two differ. A valid footer decides, unless an available sidecar
    epoch directory entry disagrees with it, in which case neither does; when
    the footer is unusable, an available sidecar epoch directory entry
    decides; when neither is available, a Recoverer whose primary copy
    validates reads the tail copy too, and two valid copies that differ leave
    the epoch metadata-unavailable. With a valid footer, the
    directory-assisted rescue is not used. A Verifier reads both copies and
    reports a divergence.
  - Section 13.3 makes `total = 2H + P + 1`, with `H > 0`, a precondition of
    the directory-assisted rescue, and states that for such an entry the tail
    copy's block is `H + P`. An entry that fails the precondition is not
    available; it affects only its epoch. The Recoverer reports
    `SidecarMetadataUnavailable` whenever no copy can be validated.
  - Section 8.4 states, in two stages, which bootstraps count as unreadable
    when values are supplied, and a Scanner given the values now MUST use them
    when it cannot read the bootstrap. A first record whose length differs
    from the supplied block size is refused. Section 15 states the bootstrap
    error names by level. Appendix D item TT-7 closes.
  - Section 2.4 defines every formula by its exact integer value: a value
    that is recorded or used must fit, and a value that is only compared is
    compared exactly. Commit-record values that do not fit are `ResumeAppend`,
    and device-reported positions that do not fit or go backwards are
    `TapeIo`. Section 3.3 states that an inverse ordinal that would not fit is
    an implicit zero. Section 7.2 requires every position a recorded map
    describes to fit in u64.
  - Section 8.4 states the rule by which a footer supplies the planned
    terminal layout: it must be where it says, and the tape must not run past
    the layout's planned EOD.
  - Section 9.6 requires the sidecar footer's `tape_uuid` to match the tape.
    Section 10.1.3 requires the ParityMap's `block_size` to equal the tape's,
    and Section 10.1.4 extends header–footer agreement to every field both
    carry.
  - Section 10.6's `W = T` rule names the recorded fields; Section 12.6
    defines a degraded inventory; Section 13.5 defines `lost_count`.
  - Section 14 refuses a committed prefix that records a terminal component
    or a final ParityMap, and names the outcome of a failed re-read.
  - Section 3.5 states that a record shorter or longer than one block is
    invalid content of its component, never `TapeIo`.
  - Sections 8.4 and 16.3 refuse recorded drive compression on a parity tape
    only; a no-parity bootstrap may record compression, as Sections 8.2 and
    11.4 already allow. Section 2.2 says that a Verifier reports damage it finds
    anywhere with the error a Reader reports for that component.
  - The owner's rulings on two further questions then add: Section 13.3
    requires a Recoverer, when the footer, the primary copy and the final
    ParityMap are all unavailable, to try the tail copy at the position the
    terminal index's record of the sidecar gives, and to use it only if it
    validates and agrees with that record. An epoch that was
    metadata-unavailable in that case can now be recovered; and Section 2.2
    says that a full verification reads every data block and parity shard, and
    that a check of structure and metadata alone is not one.
  - A later text revision states rules that the reference implementation and
    the pinned vectors already follow and that the text had left unstated:
    what a refusal and an unreadable bootstrap do in discovery with supplied
    values, which footers a Scanner reads, how the walk measures a file and
    reports a torn or wrong-length one, the constraints on the footer's and
    the header's total block count, which comparisons a replica's validity
    includes, the order of the checks in the directory-assisted rescue, what
    makes a final ParityMap valid, and what a Verifier reports of an epoch
    whose index is missing. Appendix D item TT-2 records the limits that
    follow from these rules and states the reference as it is after the
    Verifier's full verification and the sidecar pass were built. Three points
    were decided. A structural artifact after the exact terminal suffix is not
    an Object of any inventory, by scope, and the walk may still list it as a
    candidate (Section 12.6). A footer whose magic matches commits a tape file
    to its control type also when the head is readable and no head rung parses
    it (Section 12.3). A no-parity bootstrap carries no parity scheme, so that
    a scheme record in its payload is `BootstrapParse`, and the sentence of
    Section 8.2 that allowed the record to be omitted now requires its
    omission
    (Sections 8.2, 15). The first two describe what the reference already
    does. The third changes the reference, whose parser accepted such a
    bootstrap; it now refuses one. The reference's Verifier also now reports
    an unreadable ParityMap copy, which Section 2.2 requires, even when the
    other copy is used, and an unreadable ParityMap footer, or one that is a
    record of the wrong length. A footer that reads and is invalid rejects the
    ParityMap (`ParityMapParse`). Any such finding leaves the tape not
    complete. No tape byte changes.
  - Sections 15 and 17 and Appendix D item TT-2 record these changes. TT-2
    also records what the second implementation now re-derives and what
    remains open.
  - A wording pass over the text of that revision then changed no
    requirement. Section 8.4 says that a scheme record that can still be
    decoded, in a no-parity bootstrap, is refused naming the scheme when the
    supplied scheme is no parity, and that the second row of its second table
    refuses the bootstrap when the supplied scheme is a parity scheme. Section
    10.1.3 repeats Section 2.2's rule for a ParityMap footer that cannot be
    read and for one that reads and is invalid. Section 10.6 ends its second
    list with the covered-count relationship. Section 12.3 says that disjoint
    magics do not make its rungs exclusive. Section 13.3
    drops a sentence that defined a final ParityMap's validation by itself,
    and says that a validated final ParityMap excludes the tail rescue from
    the terminal index also when it has no entry for the sidecar. Section 17
    states the damage matrix as it now stands before it describes the first
    25 cases. Appendix D item TT-2 no longer lists two reference gaps that
    the reference has closed. No tape byte and no vector outcome changes.
  - A further revision of Section 12.3 changes four requirements, where the
    text left a point open or disagreed with the reference implementation. It
    is a change of requirements and not of wording. No tape byte changes. The
    reference implementation already follows the first three changes, and the
    fourth is covered below.
    - The walk with supplied values types tape file 0 by its measured length
      when its first record is unreadable: one block is a bootstrap that the
      supplied values classify and do not authenticate, and more than one block
      ends the walk with `FilemarkMapReconstruct`, because no map can hold
      anything else at tape file 0. A filemark or EOD where the first record
      should be ends the walk in the same way, and a first record of the wrong
      length is refused and not typed. The rule is stated as an exception for
      tape file 0, beside the paragraph on an unreadable head block, whose text
      is kept; that paragraph gains one sentence, for the fourth change.
    - A rung parses a file when the file passes every check of that rung except
      its measured block count, and item 1 checks that the file is tape file 0.
      A bootstrap magic at any other tape file no longer classifies the file
      as a bootstrap.
    - On the footer route, a footer whose magic matches establishes the
      control type of items 2 and 3 whether or not the footer parses. A footer
      that does not parse, or whose count differs, is reported as damage. The
      two sentences that required a fully parsed footer are rewritten, because they contradicted the rest of Section 12.3
      and Section 10.6.
    - A ParityMap is recognised by its first block and its header, or, when
      the first block is unreadable or is no other rung's kind and not a
      ParityMap header that parses, through the footer and the tail header copy
      that the footer locates. Its payload is checked when the ParityMap is
      read. A ParityMap with an intact header and a damaged payload is no
      longer an Object candidate, so it no longer changes the data ordinals of
      the Objects after it. The route through the tail copy applies the copy
      fallback of Section 10.1.3 in the walk. For a first block that reads but
      is damaged, the earlier text already recognised the ParityMap through
      that fallback, and this text keeps that outcome. For a first block that
      cannot be read, the earlier text made the file an Object candidate, and
      Appendix D recorded that the walk could not find such a ParityMap; this
      text widens recognition to that case. The paragraph on an unreadable
      head block gains the sentence that its last-block read serves this
      route.

    The reference implementation does not yet follow the tail route of the
    fourth change: it types such a file an Object candidate. A later commit
    changes it, and until then the reference and this text differ there.
    An implementation written from the earlier text agrees with this text when
    a ParityMap's first block reads but is damaged and the tail copy is intact.
    It differs when the first block cannot be read; when the first block is
    damaged and the payload is damaged too, since the tail route reads no
    payload and the earlier fallback needed one copy's payload to validate;
    and when a ParityMap's payload or footer is damaged and its first
    header is intact. It differs on the third change when a terminal footer
    does not parse, and on the first and second when tape file 0 is unreadable
    or a bootstrap magic appears at a later tape file. One open point remains, and it is recorded
    for a later revision: a readable, valid bootstrap at tape file 0 that
    measures more than one block ends the reference's walk. Section 12.3 makes
    such a file, which item 1 does not recognise, an Object candidate (item 7),
    and the 1.0.0-draft.1 entry of this appendix records the reading that a
    Scanner does not abort a whole catalog-less walk for a count mismatch.
  - The reference implementation then gained the tail route of the fourth
    change, and fourteen damage cases (`s6-01` to `s6-14`) pin the four
    changes; `s6-10`, `s6-12` and `s6-13` pin the tail route, and `s6-11` and
    `s6-14` a ParityMap that neither route recognises. Section 17 counts
    them, and Appendix D item TT-2 no longer lists the tail route as a
    reference gap. No tape byte, requirement or earlier vector outcome
    changes.
  - Section 10.1.3 now says that a ParityMap header copy whose block cannot
    be read is invalid, as one whose CRC does not verify is, so the other
    copy is used when it is valid. The text had said how a footer that cannot
    be read is treated and not a header copy. It is a change of requirements
    that states what the reference implementation and the second
    implementation already did; `s6-13` pins it. No tape byte and no vector
    outcome changes.
- **2026-08-11 — 1.0.0-draft.4 — replacement review draft.** Replaces the
  geometric/checkpoint-bootstrap design with one BOT Bootstrap and exactly
  three complete terminal index replicas separated by two typed extents.
  Adds checked streaming payload geometry, terminal capacity reservation,
  resumable five-component finalization, manual early finalization, and the
  explicit all-replicas-invalid BOT recovery taxonomy. Clarifies that optional
  fsynced host checkpoint authority may recover exact Object identifiers only
  where its tape identity, block size, committed prefix, file numbers, block
  counts, and complete Object-row bijection agree with the physical scan. This
  host-assisted classification changes no REM-PARITY media bytes. Draft.4
  remains pre-freeze pending the open evidence in Appendix E.

  The generation-2 sizes and widths, taken from a diff of the Section 2.5
  constants and the byte tables of the published revision against this one:

  - `BOOTSTRAP_SCHEMA_MAJOR` 1 to 2.
  - Bootstrap `sequence` widened from `u32` to `u64`, which moves
    `cbor_payload_len` to 0x2C, the header CRC to 0x30 and the payload to 0x38
    (`BOOTSTRAP_HEADER_LEN` 0x34 to 0x38), and raises the Bootstrap floor from
    0x3C to 0x40.
  - Sidecar `schema_version` and footer version 1 to 2.
  - Sidecar header `parity_block_count`, `data_crc_count`,
    `sidecar_header_block_count` and `inline_index_entry_bytes` widened from
    `u32` to `u64`, which moves the header CRC to 0xC0 and the inline index to
    0xC8 (`SIDECAR_HEADER_LEN` 0xB8 to 0xC8).
  - Sidecar footer `H` and `P` widened from `u32` to `u64`
    (`SIDECAR_FOOTER_LEN` 0x80 to 0x88, footer CRC 0x78 to 0x80).
  - Sidecar floor 0xD0 to 0xE0.
  - ParityMap schema and footer versions 1 to 2.
  - ParityMap `sequence` and `directory_scope_tape_file_count` widened from
    `u32` to `u64` (the other two scope fields were already `u64`), which
    moves every later header and footer field and makes the header and footer
    0xC8 bytes (CRC at 0xC0), the payload offset 0xC8, and the ParityMap floor
    0xC8.
  - Tape-file kinds 4 and 5 added, with the terminal replica and separation
    constants.

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
  revision makes these changes:

  - It replaces the draft Status text with the three-question change policy
    (invalidate or change meaning → major; earlier readers cannot identify and
    cleanly refuse → major; obligations or registry assignment → minor under
    three conditions; otherwise erratum), and states the axis-independence
    principle and the worked classifications.
  - It rewrites Section 18's preamble, which described the document as a draft
    and permitted only errata after freeze.
  - It rewrites Section 8.1.1 as the schema-minor registry (Defined-by and
    wire meanings: 2 = `object_id` optional, 3 = `object_id` required) and
    corrects the stale `1 / 3` pair in Section 2.5.
  - It states the Reader obligation at `schema_minor` ≥ 3 in Section 8.2.1 and
    the reader rule for unrecognised `flags` bits, both matching the reference
    implementation.
  - It adds the semantics-freeze rule to Section 5.3.
  - It re-anchors the key-30 overflow-carrier prohibition from the document
    version to the registry mechanism.
  - It declares the Section 8.4 discovery-candidate set frozen for the life of
    `schema_major` 1.
  - It corrects the parity_map footer magic in Sections 2.5, 5.2 and 10.3,
    which described a `PARITY_MAP_FOOTER_MAGIC_LABEL` that appears in no
    implementation and on no tape — the footer shares the header's magic.
  - It corrects the minimum sidecar block size from 0xC0 to 0xD0 (a
    generation-1 value, here and below; draft.4 raised it to 0xE0) in
    Sections 2.5, 9.1 and 9.4, and adds the index-packing rejection the
    Section 9.4 pseudocode omitted.
  - It removes an unimplementable final-directory redundancy requirement from
    Section 8.3 that contradicted Sections 10.1, 10.7 and 11.3.
  - It records in Section 12.4 the `ParityMapReference`-locator precedence
    tier and the two-candidate requirement for structural discovery, both of
    which the reference Scanner already applied.

  **If you implemented from the review draft, re-check two things.**

  - Sections 2.5, 5.2 and 10.3 described a `PARITY_MAP_FOOTER_MAGIC_LABEL`
    (`"REM\0PMAPFOOT\x01"`) as the derivation for the `parity_map` footer
    locator's magic. No such label exists: the footer carries the *header's*
    magic, `HMAC(tape_uuid, PARITY_MAP_MAGIC_LABEL)[0..8]`, on every published
    artifact and in every implementation, and it is distinguished from a
    header by its position and by `copy_kind`. An implementation built from
    the draft computes eight wrong bytes and rejects every conformant
    `parity_map` tape file, so this correction restores interoperability
    rather than removing it.
  - The minimum sidecar block size is 0xD0, not 0xC0: sizes 0xC0 through 0xCF
    satisfy the header-plus-CRC floor but cannot pack even one index entry,
    and Section 9.4's algorithm now carries the rejection that makes this
    explicit.

  No tape byte and no published vector changed for either correction.

  **Implementation conformance.** An audit of this document against the
  reference implementation, the pinned vectors, the repository documentation
  and the project website found divergences between the implementation and
  this text. Each was resolved by correcting the implementation, and this text
  was not weakened. The corrections bring the implementation to these readings
  of the text:

  - A Scanner does not abort a whole catalog-less walk when one tape file's
    recorded block count disagrees with its measured length (Section 12.3).
  - A sidecar footer that parses but contradicts the map entry is treated as
    an invalid footer, and recovery falls back to the primary header rather
    than stopping (Section 13.3 step 2).
  - An epoch is metadata-unavailable only when no header/index copy can be
    validated, directory-assisted tail rescue included (Section 13.3 steps 3
    and 4).
  - Geometry and ordinal-range disagreements raise `SchemeMismatch`, as this
    document specifies, rather than a generic parse error.
  - The REM-OBJECT manifest depth bound applies through map values as well as
    arrays.

  Two further behaviours were examined and judged conformant, on these
  readings of the text:

  - Section 12.5's guarantee, that damage to one sidecar's metadata does not
    degrade recovery of any other epoch, concerns what can be recovered. A
    request that covers several epochs may fail as a whole when one of them is
    metadata-unavailable, provided each other epoch can still be recovered on
    its own.
  - Section 8.2's requirement that a parity bootstrap's `scheme_id` be
    `rs-cauchy-gf256-v1` binds what a conformant Writer may record. A Reader
    meets its obligation by refusing an unrecognised value legibly; it may do
    so when the scheme is first used, with `SchemeMismatch`, naming the scheme
    found and the scheme expected.

- **2026-07-29 — [draft] pre-freeze revisions II.**

  - Last-resort filemark-walk operational envelope specified (Section 8.4.1),
    with inter-file positioning exempted from the Section 8.4 rule-6 abort.
  - Bootstrap re-typing promoted from SHOULD to MUST with operational
    candidate criteria (Section 12.4).
  - Appendix C items 3 and 4 resolved.

  The first two are reader-obligation changes with no effect on the set of
  valid tapes.
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
- **2026-07-22 — [draft] tape-alone recovery claims.**

  - Ordered persistence and the synchronizing barrier made normative on the
    tape I/O layer (Section 3.5).
  - Commit discipline extended with batched deferred synchronization and
    staged-record semantics (Sections 3.4, 11.1).
  - The attested prefix and the bare-tape tail taxonomy specified with salvage
    rules (Section 12.6).
  - Appendix B.8 reframed to per-file-marker rationale plus barrier-grain
    structural attestation.
- **2026-07-21 — [draft] pre-freeze revisions.**

  - Writer-legal block sizes closed over the discovery-candidate set
    (Section 8.4).
  - Object identity row keys clarified (Section 8.2.1).
  - Epochs redefined as explicit ordinal ranges with bare-counter ids, and
    short epochs legalized at any checkpoint boundary with
    `FINAL_PARTIAL_EPOCH` reserved for terminal `finish()` (Sections 3.3,
    10.5, 11.2).
  - The bootstrap directory ceiling made an admission-time refusal with
    mandatory headroom and seal-at-ceiling (Section 8.2.1).
  - Reference journal watermark note (Appendix B.12).
- **2026-06-11 — [draft] first draft.** Initial working baseline.

## Appendix D. Open Items (Informative)

This is the live preparing-copy snapshot for generation 2.

1. **TT-1 — byte derivation by the terminal verifier (candidate evidence
   passed).** The terminal verifier, a Python implementation, reproduces all
   six review profiles and their 30 component streams without calling the Rust
   codec. Incremental reviews bind later changes to the last recorded clean
   baseline; an untouched byte-contract region does not lose its accepted
   review status.
2. **TT-2 — negative and interruption vectors (candidate evidence gathered;
   REM-PARITY freeze criterion 2, recorded in `specs/README.md`, remains open
   until the items below close).**
   The review-only set verifies 50 hostile mutations, 14 survivor selections,
   68 interruption cuts, seven Object-row extension cases and the
   million-Object streaming profile; the interruption cuts are cuts in
   finalization, and Section 17's resume vectors cover append resume. It also
   holds the tape images, damage matrix, resume vectors and negative vectors
   that Section 17 describes. A second implementation written from this
   document re-derives the pinned bytes Section 17 lists and decides the cases
   Section 17 describes. Publication promotion remains gated by TT-5 and the
   other freeze criteria. The items below record what is decided and what
   remains open:
   - The second implementation now compares the mutated-block digests of the
     negative vectors (`tape-images/negatives/MANIFEST.tsv`) and decides the
     outcomes that `MUTATIONS.tsv` and `SELECTION.tsv` pin. Of the 387 digest
     rows, 340 match, and 28 are blocks a mutation leaves unchanged, whose
     digests equal the pinned ones. The other 19, in two supplement variants
     (`sidecar-real-data-shard-count/isolated-2` and
     `overflow-7.2-T/isolated`), pin bytes that this document does not
     determine: parity for an epoch longer than `S × k`, and a generated
     profile the variant leaves open. They record the reference's construction
     and are not criterion-2 items. The implementation's decisions agree with
     the Section 15 names written from this document on 49 of the 50 mutation
     rows; the fiftieth defines no bytes. `SELECTION.tsv`'s choice among valid
     replicas is the reference's policy; this document decides the outcome
     class and the set of acceptable replicas. `INTERRUPTIONS.tsv` records the
     reference Writer's states at cut points, which are implementation state
     and not format values, so criterion 2 does not ask a second
     implementation to re-derive them. At tape level, the decisions agree with
     the reference except on the five rows whose damaged payload the Scanner
     never reads, which stay informative because a Scanner need not read every
     payload (Section 12.6), and `gap-wrong-total-length`, whose outcome is
     the BOT walk (Section 8.4 step 1). The four conflict rows of
     `SELECTION.tsv` have been rebuilt so that each conflicting replica can be
     eligible, and the reference returns `TerminalIndexReplicaConflict` for
     each. The second implementation's decisions for `e3-02` and `e3-06`
     follow Section 12.6's scope rule.
   - Five formulas blocked freeze under the release record's minimum coverage
     for negative candidates, whose last item asks for overflow in every size
     and location formula, because this document left the outcome of their
     overflow undefined. It now decides each. Section 3.2's `LBA(f, b)` must
     fit for a map to be valid (Section 7.2). Section 3.3's inverse compares
     the exact ordinal, so an ordinal that would not fit is an implicit zero
     (Sections 2.4 and 3.3). The append point's inputs come from commit
     records, so its overflow is `ResumeAppend`, and the walk length's inputs
     are device reports, so its overflow is `TapeIo` (Section 2.4). Section
     13.3's tail location is `H + P`, with the precondition
     `total = 2H + P + 1` and `H > 0`. These stay open until their vectors are
     pinned to these outcomes. The `freeze_blocker` flags in
     `negative-cases.json` predate this classification; where they differ,
     this item governs, until the flags are cleared.
   - No vector can exist for four formulas, and they do not block freeze for
     that reason. No input a Reader reads can make Section 10.1.2's `2M + 1`
     overflow at a legal block size, and no operation in Section 3.3's forward
     mapping or in Section 9.4's computation of `H` can overflow. Only a
     Writer evaluates Section 10.3's close-reserve formula, from its own
     state.
   - Three formulas have a pinned overflow vector that the reference rejects
     at an earlier check: Section 7.2's `T`, Section 10.4's record-geometry
     product and Section 10.5's `actual_bytes`. Under Section 2.4's exact-value
     rule, three pinned vectors describe inputs whose exact values fit, and
     are accepted: `overflow-10.1.2-M/isolated`,
     `overflow-10.4-record-geometry/isolated` and
     `overflow-10.3-manifest-range/isolated-2`. Their expectations are to be
     revised, and every other pinned overflow vector checked against the rule.
     Whether Section 10.1.2's `M`, Section 10.4's record geometry and Section
     10.3's manifest capacity still have an overflow that a vector can reach
     under the exact-value rule is to be recorded when those vectors are
     revised: each either keeps a reachable vector or joins the formulas for
     which none can exist.
   - Of the four questions this item recorded, all four are now decided: a
     rung that fails on a count mismatch does not recognise the file (Section
     12.3; `filemark-prefix`); a sidecar's footer or directory decides between
     two copies that differ, and a Verifier reports the divergence (Sections
     9.1 and 13.3; `sidecar-primary-tail-disagreement`); which bootstraps
     count as unreadable (Section 8.4; TT-7, now closed); and when the footer,
     the primary copy and the final ParityMap are all unavailable, a Recoverer
     tries the tail copy that the terminal index's record of the sidecar
     locates (Section 13.3; `parity-map-and-sidecar`). The reference has that
     route. The tail
     rescue's formula `H = (total − 1 − P) / 2` keeps a vector, as the
     formulas above do: a replica row for a sidecar with a block count of at
     most `P` makes `total − 1 − P` negative, and the epoch is then
     metadata-unavailable (Section 13.3), not a rejection of the replica.
   - The three readings this item listed are now stated: exact integer values
     (Section 2.4), the recorded fields in Section 10.6's `W = T` rule, and the
     tail copy at `H + P`, with a directory entry's total equal to
     `2H + P + 1` (Section 13.3).
   - Section 2.2 includes in a Verifier's full check the Recoverer's index and
     CRC validation. The reference's verification reads a sidecar's footer and
     two header copies, and every data block it protects and every parity
     shard it holds, and checks each against the index. A full
     verification reads every data block and parity shard (Section 2.2), and
     the reference's verification does so.
   - Where this document leaves a name open, some vectors give a set of
     permitted Section 15 errors, or none. The second implementation's
     `GAPS.md` and the notes in `tape-images/negatives/` list these and the
     other places where this document is silent or ambiguous. Each is to be
     decided, or listed here, before freeze.
   - Section 12.3 (item 4) finds a ParityMap whose block 0 is unreadable, or
     damaged so that no head rung takes it, through its footer and tail header
     copy, and the replica route survives that damage by locating it through
     structural rows. A ParityMap for which neither the header nor the footer
     route works is an Object candidate. The reference implementation follows
     both routes; the damage cases `s6-10`, `s6-12` and `s6-13` pin the footer
     route.
   - Section 12.3 leaves one point open. A readable, valid bootstrap at tape
     file 0 that measures more than one block is not recognised by item 1, and
     the reference implementation's walk ends at once with
     `FilemarkMapReconstruct`, because a map cannot hold anything but a
     one-block bootstrap at tape file 0. That differs from Section 12.3, which
     makes such a file an Object candidate (item 7), and from the 1.0.0-draft.1
     entry of Appendix C, which reads that a Scanner does not abort a whole
     catalog-less walk for a count mismatch. No vector pins it.
   - For item 6, a footer whose tail copy disagrees with it is reported and the
     file falls through, although the definition of "parses" would make that
     footer not parsed. No outcome turns on it, because item 6 is not part of
     the footer rule's gate.
   - Section 12.3's rule for tape file 0 speaks only of the walk with supplied
     values. Without them, the reference types an unreadable one-block tape
     file 0 a bootstrap as well, and this document leaves that mode
     unspecified.
   - Section 12.3 reports as damage a terminal footer that does not parse or
     whose count differs. Whether a replica whose head is unreadable but whose
     footer is good counts as damaged for Section 12.6's test of the exact
     terminal suffix is not stated.
   - A bootstrap of another `schema_major` is refused by name
     (`BootstrapParse`, format major) only when values are supplied. Without
     them, discovery reports `NoBootstrapFound`, because Section 15 gives
     discovery no other outcome for a bootstrap it cannot use.
   - A sidecar header whose metadata is wrong but whose checksums are valid
     makes the walked map's digest check fail, and so denies every epoch on
     the walk route. Section 13.1's check is all-or-nothing by design, and the
     replica route is not affected.
   - A Reader loads a ParityMap tape file of any length into memory; bounding
     that load belongs with the fuzzing campaign of TT-6.
   - A Verifier reports every finding it makes about one sidecar. This
     document does not order them.
   - The walk cannot type a terminal replica or separation extent none of
     whose records can be read. With no head and no footer to read, the file
     is an Object candidate outside the validated scope (Section 12.3),
     although the planned tuples in a readable neighbour name it.
   - No rule compares a directory entry's `parity_shard_block_count` with `S ×
     m`. The rescue takes `P` from the scheme, so an entry whose own count
     disagrees changes nothing a Reader does.
   - When a final ParityMap validates and its entry for a sidecar fails a
     precondition of the directory-assisted rescue, an intact tail copy is not
     read, and the epoch is metadata-unavailable, although the same tape would
     recover on the replica route if the ParityMap were lost.
   - A medium error at a filemark position has no outcome in this document.
     The vectors that list such a position take the filemark as present.
   - The bootstrap vectors all use the first candidate block size. A supplied
     size that is not the first candidate is covered by unit tests only.
   - A Verifier that finds physical damage in the prefix keeps the terminal
     route only while every undamaged file of the walked prefix agrees with
     the replica's rows in every field it records. A damaged sidecar that the
     walk types as an Object changes the recounted ordinals of later Objects,
     and the Verifier then takes the walk; this is conservative and loses no
     block.
   - A claim in `fixtures/rem-parity-1/vectors.json` still names the
     authoritative directory overlays that generation 2 removed; nothing reads
     the file.

   The generation-1 REM-PARITY cases of the pinned archive were read against
   generation 2. Most of the properties they test are kept. The reference
   implementation's tests cover them, and the generation-2 candidate vectors
   now also cover the groups that had no candidate coverage, among them the
   sidecar negatives.
3. **TT-3 — media exercise of the default separation extents (VTL passed;
   physical open).** The exact one-GiB layout has passed clean VTL writes and
   independent verification of the terminal suffix at all three legal block
sizes. The 256 KiB
   and 512 KiB legs respectively proved 4,096 and 2,048 records per separation
   extent, compression disabled, the five dense tape files of the terminal
   suffix, their filemarks, and exact EOD. At least two supervised
   physical-tape block sizes are still required, including footer,
   filemark/EOD, PEW, and EOM observations.
4. **TT-4 — end-to-end lifecycle reconciliation (implemented and rerun).** The
   VTL scenarios have exercised automatic and manual finalization, all 68
   interruption cuts, permanent Object refusal after finalization starts,
   eventual reconciliation of the complete terminal suffix, sole-BOT
   checkpoint replay, and the full matrix of default separation extents. Each
   affected member ran from its own clean slate at the recorded implementation
   head.
5. **TT-5 — external prose and incremental-diff review.** The preparing copy
   has no normative dependence on geometric placement, `2M+1` index copies,
   bootstrap Object-row ceilings, or singular final bootstrap authority.
   Review is incremental from the last recorded clean baseline; two
   consecutive clean reviews of the then-current increment satisfy the gate,
   and later commits reopen only the regions they touch.
6. **TT-6 — terminal-format fuzz plateaus (open).** Existing campaigns cover
   bootstrap, sidecar, ParityMap, and the pre-terminal scan walk. They predate
   the generation-2 terminal format introduced in draft.4: there is not yet a
   dedicated coverage-guided
   terminal-replica parser campaign, separation parser campaign, or scan-walk
   campaign whose generator reaches terminal kinds at the legal 256 KiB,
   512 KiB, and 1 MiB record sizes. REM-PARITY freeze criterion 3, recorded in
   `specs/README.md`, remains open until those targets, committed corpus
   replay, and measured plateau reports exist.
7. **TT-7 — which bootstraps count as unreadable (closed).** Section 8.4 now
   states it, in two stages: the physical read, then the content of a record
   of the right length. Section 15 states the bootstrap error names by the
   level at which a Reader meets the block.

## Author's Address

The ArchiveTech Project
Website: https://archivetech.org
Email: specs@archivetech.org
Reference implementation: https://github.com/archivetechie/remanence
