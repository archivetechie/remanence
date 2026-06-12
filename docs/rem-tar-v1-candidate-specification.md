# rem-tar-v1

## Candidate Specification

| | |
| --- | --- |
| Status | Candidate (draft for review) |
| Revision | 1 |
| Date | 2026-06-10 |
| Format identifier | `rem-tar-v1` |
| Schema version | `1.0` |
| Reference implementation | Remanence (`crates/remanence-format`) |
| Supersedes | `rem-tar-v1-design.md` v0.9.3 §7–§8 (wire format); reconciles `spec-v0.4.md` §8.7 |

> **Drafting note (remove at freeze):** this document is the normative
> fixed point; the implementation is validated against it, not the
> reverse. The wire-format description matches what the reference
> implementation emits as of the 2026-06-10 working tree (including the
> post-review fixes: RFC 8949 manifest key order, reader format gates,
> duplicate-entry validation, incomplete-block-write detection). The
> behavioral requirements deliberately go beyond the implementation
> where the implementation is known to be weak; Appendix B is the
> conformance backlog, and closing it is a freeze criterion.

## Abstract

This document specifies `rem-tar-v1`, the native on-tape body format for
Remanence objects. A `rem-tar-v1` object is a single, complete POSIX pax
tar archive occupying exactly one tape file, written as fixed-size body
blocks. The format constrains standard pax tar in one structural way —
every file's payload begins on a body-block boundary — and extends it
with vendor pax keywords (`REMANENCE.*`) and a generated CBOR manifest
stored as the archive's final member. The constraints preserve full
extractability by standard tar tools while enabling closed-form
byte-range addressing (partial file restore), per-file cryptographic
identity, and catalog-less recovery. The format is designed so that a
future implementer holding only this document and its static test
vectors can read every conformant object, and so that an operator
holding only standard `tar` can recover every payload byte.

## Table of Contents

1. [Introduction](#1-introduction)
2. [Conventions and Terminology](#2-conventions-and-terminology)
3. [Object Layout](#3-object-layout)
4. [The ustar Record Subset](#4-the-ustar-record-subset)
5. [Pax Extended Records](#5-pax-extended-records)
6. [The Global Header](#6-the-global-header)
7. [File Entries and Block Alignment](#7-file-entries-and-block-alignment)
8. [The Object Manifest](#8-the-object-manifest)
9. [End of Archive](#9-end-of-archive)
10. [Writer Obligations](#10-writer-obligations)
11. [Reader Obligations](#11-reader-obligations)
12. [Versioning and Extensibility](#12-versioning-and-extensibility)
13. [Errors](#13-errors)
14. [Security Considerations](#14-security-considerations)
15. [Interoperability and the 30-Year Fallback](#15-interoperability-and-the-30-year-fallback)
16. [Test Vector Requirements](#16-test-vector-requirements)
17. [Candidate Freeze Criteria](#17-candidate-freeze-criteria)
18. [References](#18-references)

Appendix A. [Decisions and Superseded Material](#appendix-a-decisions-and-superseded-material)
Appendix B. [Reference Implementation Gaps](#appendix-b-reference-implementation-gaps)
Appendix C. [Worked Alignment Example](#appendix-c-worked-alignment-example)

---

## 1. Introduction

### 1.1. Purpose and Design Goals

`rem-tar-v1` wraps a set of named file payloads into one archival object
stored on linear tape. Its design goals, in priority order:

1. **Tar validity is non-negotiable.** The byte stream is a fully valid
   POSIX pax tar archive. A standard pax-aware `tar` extracts every
   payload byte-correct with no Remanence software present. No zero
   padding inside a file's payload; no inflated tar header sizes.
2. **Closed-form byte-range addressing.** Every file's payload begins
   exactly on a body-block (`chunk_size`) boundary, so the block
   containing payload byte `b` of a file is
   `first_chunk_lba + floor(b / chunk_size)` — no scanning, no
   decompression, no per-file index beyond two integers.
3. **Per-file cryptographic identity.** Every entry carries the SHA-256
   of its exact payload bytes, in both its pax header and the object
   manifest, making the externally-anchored trust chain
   (bootstrap → manifest → file) reconstructible from tape alone.
4. **Deterministic byte representation.** Given identical inputs and
   options, every conformant writer produces the identical byte stream,
   and the generated manifest is byte-identical across implementations.
5. **Long-term recoverability.** The format is recoverable from this
   document plus its static test vectors, and — degraded — from
   knowledge of POSIX tar alone.

### 1.2. Relationship to Remanence Layers and Adjacent Formats

`rem-tar-v1` is Layer 3b of the Remanence architecture
(`docs/spec-v0.4.md` §8). The division of responsibility:

- **`rem-tar-v1` owns** the bytes of one object archive: tar framing,
  alignment, vendor keywords, and the manifest.
- **Layer 3c (`rem-parity`) owns** everything outside the archive
  bytes: the tape filemark terminating the object's tape file, parity
  sidecars, block-level CRCs, the BOT bootstrap (which stores each
  object's manifest location and `manifest_sha256`), and the durable
  commit barrier. An object is *committed* when Layer 3c's
  `finish_object()` returns; this format defines no in-band commit
  marker.
- **Layer 4/5 own** catalogs, object selection, and restore policy,
  including path sanitization at restore time (Section 14.4).
- **Payload contents are opaque.** In the intended deployment the
  payloads are AOF1 objects ([AOF1]); `rem-tar-v1` neither knows nor
  cares. Payload-level encryption and compression live in the payload
  format, not here.

### 1.3. Non-Goals

`rem-tar-v1` performs no compression (Section 12.3), no encryption, and
no self-authentication (Section 14.1). It defines no multi-object
container: one object is one archive is one tape file. It does not
define how `chunk_size` is chosen (that is tape-geometry policy,
Layer 3a/3c) and it stores no parity.

Version 1.0 of the format encodes **regular files only**. Symbolic
links, hard links, directory entries, and special files are not
representable in the 1.0 wire format; the manifest reserves the fields
where a 1.x minor revision will add them (Section 12.2). The symlink
classification policy designed in `spec-v0.4.md` §8.7.7 is the intended
1.x semantics and is not normative here.

## 2. Conventions and Terminology

### 2.1. Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and
"OPTIONAL" in this document are to be interpreted as described in BCP 14
[RFC2119] [RFC8174] when, and only when, they appear in all capitals.

### 2.2. Conformance Targets

- **Writer**: produces `rem-tar-v1` object byte streams (Section 10).
- **Planner**: computes an object's exact layout and block count
  without payload bytes, for capacity admission (Section 10.2). A
  Planner MUST produce byte-for-byte the layout the Writer then emits.
- **Reader**: parses object byte streams and recovers entries
  (Section 11). Two I/O profiles exist — streaming and materializing —
  with identical acceptance rules, and two operating modes — restore
  (the default; integrity-verifying) and salvage (Section 11.1).
- **Verifier**: validates a complete object end to end — framing,
  every payload digest, the manifest digest, and the
  manifest-vs-archive cross-check — without extracting payloads
  (Section 11.4).
- **Scanner**: walks an archive using only POSIX tar knowledge, as the
  degraded fallback (Section 15.2). Scanners are not required to
  implement this document.
- **Consumer**: interprets a decoded manifest (Section 8).

### 2.3. Definitions

- **Object**: one `rem-tar-v1` archive; the unit of write, commit, and
  restore.
- **Body block / chunk**: a fixed-size block of `chunk_size` bytes; the
  unit in which the archive is written to tape. The terms are synonyms;
  "chunk" is used for addressing, "block" for I/O.
- **`BodyLba`**: zero-based index of a body block within one object.
  Each object's `BodyLba` starts at 0; pairing it with the object's
  tape-file number forms a complete tape address. `rem-tar-v1`
  addresses exclusively in `BodyLba` (`spec-v0.4.md` §8.7.1).
- **Record**: a 512-byte tar record. All tar structures are sequences
  of records.
- **Entry**: one pax extended header + one regular-file ustar header +
  payload + record padding, describing one file.
- **Payload file**: a caller-supplied entry. **Manifest**: the
  generated final entry (Section 8).
- **Effective path / effective size**: the entry's path and size after
  applying pax overrides (Section 5.4).

### 2.4. Integer, Byte, and Text Conventions

Byte offsets are zero-based. `KiB` = 2^10 bytes. ustar numeric fields
are ASCII octal (Section 4.2). Pax values and CBOR integers are decimal
/ binary respectively. All text in the format — pax keywords and
values, paths, manifest text strings — is UTF-8; pax keywords are
additionally restricted to ASCII. All derived quantities (offsets,
block counts, chunk counts) are defined over unsigned 64-bit
arithmetic; implementations MUST use checked arithmetic and MUST NOT
wrap silently (Section 13).

### 2.5. Constants

| Constant | Value | Meaning |
| --- | --- | --- |
| `TAR_RECORD_SIZE` | 512 | POSIX tar record size in bytes |
| `DEFAULT_CHUNK_SIZE` | 262144 (256 KiB) | Default body-block size |
| `FORMAT_ID` | `rem-tar-v1` | Value of `REMANENCE.format_id` |
| `SCHEMA_VERSION` | `1.0` | Value of `REMANENCE.schema_version` (Section 12.1) |
| `MANIFEST_PATH` | `_remanence/manifest.cbor` | Manifest entry path |
| `RESERVED_PREFIX` | `_remanence` | Reserved path namespace (Section 7.6) |
| `USTAR_SIZE_MAX` | 0o77777777777 (8 GiB − 1) | Largest size representable in the ustar size field |
| `PAX_PATH_PLACEHOLDER` | `remanence/pax-path` | ustar name placeholder for pax-backed paths (Section 4.3) |
| `GLOBAL_HEADER_NAME` | `GlobalHead.0/PaxHeaders/remanence` | ustar name of the global pax header |
| `PAX_HEADER_NAME` | `PaxHeaders.0/remanence_file` | ustar name of payload-file pax headers |
| `MANIFEST_PAX_HEADER_NAME` | `PaxHeaders.0/_remanence_manifest` | ustar name of the manifest's pax header |
| `MANIFEST_SCHEMA_VERSION` | 1 | Manifest CBOR `schema_version` integer (Section 8.3) |
| `MAX_FILE_ENTRIES` | 10000000 | Maximum payload entries per object (Section 8.2) |
| `MANIFEST_MAX_DEPTH` | 8 | Maximum manifest CBOR nesting depth (Section 8.2) |

## 3. Object Layout

### 3.1. Frame Sequence

A `rem-tar-v1` object is a byte string with the following layout, where
every frame boundary falls on a 512-byte record boundary:

```text
+--------------------------------------------------+
| Global pax header (typeflag 'g')                 |  Section 6
+--------------------------------------------------+
| Entry 0:  pax header ('x') + ustar ('0') + data  |  Section 7
+--------------------------------------------------+
| ...                                              |
+--------------------------------------------------+
| Entry N-1 (last payload file)                    |
+--------------------------------------------------+
| Manifest entry ('x' + '0' + CBOR data)           |  Section 8
+--------------------------------------------------+
| Tar EOF: two all-zero 512-byte records           |  Section 9
+--------------------------------------------------+
| Zero fill to the next chunk_size multiple        |  Section 9
+--------------------------------------------------+
```

The manifest entry MUST be the final entry before tar EOF. Payload
entries appear in caller-supplied order. An object with zero payload
files is valid: it contains the global header, the manifest entry, and
the EOF sequence.

### 3.2. Body Blocks and `chunk_size`

The object byte stream is written as consecutive fixed-size body blocks
of exactly `chunk_size` bytes. `chunk_size` MUST be a positive multiple
of 512. `chunk_size` MUST equal the fixed tape block size of the
containing tape file; one body block is one tape block. The format
defines no maximum; operational bounds come from drive block-size
limits (Layer 3a).

The total object length is always an exact multiple of `chunk_size`
(Section 9.2), and the object's block count is knowable before any
payload byte is written (Section 10.2).

### 3.3. Object Boundary

A `rem-tar-v1` object comprises exactly the frames of Section 3.1 and
is externally delimited: the containing layer (3c) writes the object's
blocks as one tape file and terminates it with a filemark. The format
reserves no meaning for bytes outside the object's blocks and provides
no in-band mechanism for locating object boundaries. A Reader is given
the object's `chunk_size` and block count out of band (catalog,
bootstrap, or filemark map) and MUST process exactly that many blocks.

## 4. The ustar Record Subset

`rem-tar-v1` emits POSIX ustar headers [POSIX-PAX] restricted as
specified here. Readers MUST validate the checksum of every non-zero
header record (Section 4.4) and SHOULD be liberal in fields this
section marks reader-ignored.

### 4.1. Header Layout

Every header is one 512-byte record:

| Offset | Length | Field | Writer-normative value |
| ---: | ---: | --- | --- |
| 0 | 100 | `name` | Entry-dependent (Section 4.3); NUL-padded |
| 100 | 8 | `mode` | `0000644\0`, or `0000755\0` when `REMANENCE.executable` is `true` |
| 108 | 8 | `uid` | `0000000\0` |
| 116 | 8 | `gid` | `0000000\0` |
| 124 | 12 | `size` | 11 octal digits + NUL (Section 4.3) |
| 136 | 12 | `mtime` | `00000000000\0` (mtime lives in pax only, Section 5.5) |
| 148 | 8 | `chksum` | Section 4.4 |
| 156 | 1 | `typeflag` | `g`, `x`, or `0` (Section 4.5) |
| 157 | 100 | `linkname` | All NUL |
| 257 | 6 | `magic` | `ustar\0` |
| 263 | 2 | `version` | `00` |
| 265 | 32 | `uname` | `remanence`, NUL-padded |
| 297 | 32 | `gname` | `remanence`, NUL-padded |
| 329 | 8 | `devmajor` | All NUL |
| 337 | 8 | `devminor` | All NUL |
| 345 | 155 | `prefix` | All NUL (writers never use `prefix`) |
| 500 | 12 | — | All NUL |

Octal fields are zero-padded ASCII octal terminated by NUL. When
parsing, Readers MUST stop a numeric field at the first NUL or space,
MUST accept surrounding ASCII whitespace, and MUST treat an empty field
as zero.

The `uid`, `gid`, ustar `mtime`, `uname`, and `gname` fields carry the
fixed values above regardless of metadata-preservation tier: `rem-tar-v1`
1.0 deliberately does not preserve ownership, so a root-run standard
`tar` extraction cannot apply ownership the format never recorded
(`spec-v0.4.md` §8.7.8). Readers MUST ignore these fields.

### 4.2. Reader-Ignored Fields

Readers MUST NOT base acceptance on `mode`, `uid`, `gid`, ustar
`mtime`, `uname`, `gname`, `devmajor`, `devminor`, or `version`.
Readers MUST honor `prefix` when forming a header path from a foreign
ustar header (`prefix + "/" + name` when `prefix` is non-empty), even
though conformant writers leave it empty.

### 4.3. Names and Sizes: Pax-Backed Placeholders

The authoritative path and size of an entry are its pax `path` and
`size` records when present (Section 5.4). The ustar header
nevertheless remains well-formed:

- **Name.** If the effective path is non-empty, at most 100 bytes, and
  consists solely of non-control ASCII, the writer MUST store it in
  `name` verbatim. Otherwise the writer MUST store
  `PAX_PATH_PLACEHOLDER` (`remanence/pax-path`).
- **Size.** If the payload length is ≤ `USTAR_SIZE_MAX`, the writer
  MUST store it in `size`. Otherwise the writer MUST store zero. (The
  pax `size` record is always present and authoritative, so files
  ≥ 8 GiB are fully supported.)
- The ustar names of pax header records themselves are the fixed
  constants `GLOBAL_HEADER_NAME`, `PAX_HEADER_NAME`, and
  `MANIFEST_PAX_HEADER_NAME` (Section 2.5). These names exist for
  human-readable `tar -t` listings; Readers MUST NOT interpret them.

### 4.4. Checksum

The `chksum` field holds the unsigned sum of all 512 header bytes with
the eight checksum bytes treated as ASCII spaces (0x20), encoded as six
ASCII octal digits, a NUL, and a space. Writers MUST emit exactly this
encoding. Readers MUST verify the unsigned checksum and reject a
mismatch with `UstarChecksumMismatch`.

### 4.5. Typeflags

| Typeflag | Meaning |
| --- | --- |
| `g` (0x67) | Global pax header (Section 6) |
| `x` (0x78) | Per-entry pax extended header (Section 5) |
| `0` (0x30) | Regular file |
| NUL (0x00) | Accepted by Readers as a regular file (pre-POSIX compatibility); writers MUST NOT emit it |

Readers MUST reject any other typeflag with `UnsupportedTarTypeflag`.
This is deliberate: version 1.0 has no link, directory, or special
entries (Section 1.3), and accepting them silently would misrepresent
an unsupported archive as fully restored.

## 5. Pax Extended Records

### 5.1. Record Grammar

A pax header's payload is a sequence of records, each:

```text
"<len> <keyword>=<value>\n"
```

where `<len>` is the decimal byte length of the entire record including
the length digits themselves, the single space, and the trailing
newline. `<len>` is self-referential; writers MUST compute it by
fixed-point iteration over its own digit count (the value is unique;
crossing a decimal-digit boundary — 9→10, 99→100, 999→1000 — changes
`<len>` by one and the iteration converges).

Constraints, enforced by both Writers and Readers:

- `<keyword>` MUST be non-empty ASCII and MUST NOT contain `=`,
  newline, or NUL.
- `<value>` MUST be valid UTF-8 and MUST NOT contain any byte < 0x20
  (this excludes newline by construction; a pax value in this format
  is always single-line).
- `<len>` MUST be ≥ 1 and MUST NOT exceed the remaining header payload.
- The record MUST end with exactly one newline at offset `<len> − 1`.

Readers MUST reject violations with `PaxRecordMalformed`.

### 5.2. Emission Order and Duplicates

Writers MUST emit the records of one pax header sorted in ascending
bytewise order of the keyword, and MUST NOT emit the same keyword twice
in one header. (Note the consequence: all `REMANENCE.*` keywords sort
before the lowercase standard keywords `mtime`, `path`, `size`.)

Readers MUST apply POSIX last-wins semantics if duplicates are
encountered in a foreign archive, and MUST NOT reject an archive solely
for unsorted records. Determinism (design goal 4) is a writer
obligation, not a read-acceptance rule.

### 5.3. Unknown Keywords

Readers MUST ignore unknown keywords, including unknown `REMANENCE.*`
keywords (this is how 1.x minor revisions extend the format,
Section 12.2), and SHOULD preserve them when re-emitting metadata.
Unknown keywords MUST NOT alter payload framing or interpretation.

### 5.4. Standard Keywords Used

| Keyword | Presence | Meaning |
| --- | --- | --- |
| `path` | REQUIRED on every entry | Effective UTF-8 entry path; overrides the ustar `name` |
| `size` | REQUIRED on every entry | Effective payload byte length (decimal); overrides the ustar `size` |
| `mtime` | OPTIONAL | Modification time in POSIX pax decimal form: non-negative decimal seconds since the epoch, optionally followed by `.` and fractional digits. Writers MUST validate this shape; a non-pax-conformant `mtime` would corrupt standard-tool extraction semantics |

Writers MUST always emit `path` and `size` even when the ustar header
could carry them, so that every entry is self-describing under pax
rules alone. Readers MUST use the pax values when present and fall back
to the ustar fields otherwise (foreign-archive tolerance).

### 5.5. Metadata Preservation

The object-level tier (`REMANENCE.metadata_preservation`, Section 6.1)
declares intent: `minimal` (path + content), `archival` (adds `mtime`
and `REMANENCE.executable`), `full` (reserved). In wire terms, version
1.0 defines exactly the `mtime` and `REMANENCE.executable` carriers;
the `full` tier value is valid but adds no additional 1.0 wire fields
(ownership keywords are reserved for a 1.x revision, Section 12.2).
Presence of `mtime`/`REMANENCE.executable` on an entry is governed by
the caller, not validated against the declared tier.

## 6. The Global Header

### 6.1. Keywords

The first record group of every object is a global pax header
(typeflag `g`) whose payload carries exactly these eight keywords
(emitted in bytewise keyword order, Section 5.2):

| Keyword | Constraint |
| --- | --- |
| `REMANENCE.caller_object_id` | Non-empty opaque UTF-8; orchestrator-assigned identifier |
| `REMANENCE.chunk_size` | Decimal `chunk_size` in bytes; MUST equal the body-block size of the containing tape file |
| `REMANENCE.encryption` | MUST be `none`; any other value requires a new `format_id` (Sections 12.2, A.5) |
| `REMANENCE.format_id` | MUST be `rem-tar-v1` |
| `REMANENCE.metadata_preservation` | One of `minimal`, `archival`, `full` |
| `REMANENCE.object_id` | Non-empty UTF-8 object identifier (a UUID string in practice; opaque to this format) |
| `REMANENCE.schema_version` | `<major>.<minor>` decimal text; MUST have major version 1 (Section 12.1) |
| `REMANENCE.write_timestamp` | RFC 3339 timestamp of object creation |

### 6.2. Validation

Before delivering any entry, a Reader MUST verify on the accumulated
global records:

1. `REMANENCE.format_id` is present and equals `rem-tar-v1`
   (`UnsupportedFeature` otherwise; a missing key is `Parse`).
2. `REMANENCE.schema_version` is present and its major component
   (the decimal text before the first `.`, or the whole value if no
   `.`) parses as an unsigned integer equal to 1 (`UnsupportedFeature`
   on mismatch, `Parse` on malformed).
3. If `REMANENCE.encryption` is present, it equals `none`
   (`UnsupportedFeature` otherwise). This is the refusal gate of
   Appendix A.5: a reader that ignored it could restore ciphertext as
   content under a future revision.
4. If `REMANENCE.chunk_size` is present, it equals the externally
   supplied `chunk_size` (`ChunkSizeMismatch` otherwise). A mismatch
   means the object is mis-cataloged or the tape was rewritten with
   different geometry; alignment checks alone do not reliably catch
   every wrong-geometry combination, and restoring under the wrong
   geometry mis-addresses every chunk.

The remaining global keywords (`object_id`, `caller_object_id`,
`metadata_preservation`, `write_timestamp`) are descriptive; Readers
MUST NOT require them for acceptance. Consumers cross-check the
identity keywords against the manifest per Section 8.3.

A conformant writer emits exactly one global header, first. Readers
MUST accept a foreign archive containing additional `g` headers later
in the stream by merging their records with last-wins semantics and
re-running the Section 6.2 checks before the next entry is delivered.

## 7. File Entries and Block Alignment

### 7.1. Entry Frame

Each entry is, in order:

1. A pax extended header: ustar record (typeflag `x`) whose `size` is
   the pax payload length; the pax payload; zero padding to the next
   512-byte boundary.
2. A regular-file ustar header (typeflag `0`, Section 4.3).
3. The payload: exactly `size` bytes (the effective size).
4. Zero padding to the next 512-byte boundary (none if
   `size mod 512 = 0`).

### 7.2. Per-Entry Keywords

In addition to `path`, `size`, and optional `mtime` (Section 5.4),
every entry's pax header carries:

| Keyword | Presence | Constraint |
| --- | --- | --- |
| `REMANENCE.chunk_count` | REQUIRED | Decimal; MUST equal the Section 7.4 value |
| `REMANENCE.compression` | REQUIRED | MUST be `none` in 1.0 (Section 12.3) |
| `REMANENCE.executable` | OPTIONAL | `true` or `false` |
| `REMANENCE.file_id` | REQUIRED | Non-empty opaque UTF-8 stable identifier, unique within the object |
| `REMANENCE.file_sha256` | REQUIRED | Exactly 64 lowercase hex digits: SHA-256 of the exact payload bytes |
| `REMANENCE.is_manifest` | Manifest entry only | MUST be `true`; MUST be absent on payload entries |
| `REMANENCE.pad` | As needed | Alignment filler (Section 7.3); value MUST consist solely of ASCII spaces |

Readers MUST verify `REMANENCE.compression` is present and equals
`none` on every entry before delivering its payload
(`UnsupportedFeature` otherwise; missing key is `Parse`). Readers MUST
ignore the *content* of `REMANENCE.pad`. Readers SHOULD cross-check
`REMANENCE.chunk_count` against the value recomputed from the
effective size (Section 7.4) and surface a mismatch as an
inconsistency, since pax bodies carry no checksum of their own at this
layer (Section 14.2). `REMANENCE.file_sha256` is not consulted during
framing; its verification is a delivery-time obligation
(Section 11.2) and the core of the Verifier profile (Section 11.4).

### 7.3. The Alignment Rule

**Invariant (normative):** for every entry, the payload start offset
`D` (the first byte after the regular ustar header) satisfies

```text
D ≡ 0   (mod chunk_size)
```

This holds for *every* entry — including zero-length entries and the
manifest — keeping the layout rule uniform and the planner
deterministic. Readers MUST reject an entry whose effective size is
greater than zero and whose payload offset is not chunk-aligned with
`ChunkAlignmentViolation`. (For zero-length entries no payload exists,
so the invariant is writer-side only.)

Alignment is achieved entirely inside the entry's own pax header by
sizing the `REMANENCE.pad` record. Let `O` be the byte offset of the
entry's pax ustar record (always a multiple of 512), `B` the pax
payload length after including the pad record, and
`R = roundup512(B)`. The writer MUST choose the pad length such that

```text
O + 512 + R + 512 ≡ 0   (mod chunk_size)
```

(the two 512s are the pax ustar record and the file ustar record).

Because the byte stream must be deterministic (design goal 4), the pad
length is uniquely determined, not merely any solution. The normative
selection rule: let `Rmin` be `roundup512` of the pax payload including
an empty-valued pad record; the target `R` is the smallest multiple of
512 that is ≥ `Rmin` and satisfies the congruence; the pad value is the
largest number of spaces for which the pax payload length does not
exceed `R` (solving each candidate record's self-referential length per
Section 5.1). If that payload does not round up to exactly `R` (a
decimal-digit-boundary corner), the writer advances `R` by `chunk_size`
and retries; a writer MUST fail with `Layout` rather than emit a
misaligned entry if no solution exists within `4 × chunk_size` above
`Rmin` (none is known to be unreachable in that bound). The pad is
never a standalone tar member — it is a legitimate pax record of the
entry it aligns, invisible to standard tools.

There is no padding *after* payloads beyond tar's normal 512-byte
record padding: a body block may contain one file's tail bytes followed
immediately by the next entry's headers. Readers recover exact sizes
from `size`, never from block boundaries.

### 7.4. Chunk Geometry

For an entry with effective size `Z` and payload offset `D`:

```text
chunk_count     = 0                       if Z = 0
                  ceil(Z / chunk_size)    otherwise
first_chunk_lba = absent                  if Z = 0
                  D / chunk_size          otherwise
```

`first_chunk_lba` is a `BodyLba`. Byte range `[s, s+n)` of the file
maps to body blocks
`first_chunk_lba + floor(s / chunk_size) ..= first_chunk_lba + floor((s+n−1) / chunk_size)`
with head/tail trimming; the final chunk holds
`Z − (chunk_count−1) × chunk_size` payload bytes (plus whatever follows
in the stream). Range requests MUST be validated against `Z` with
checked arithmetic before mapping.

### 7.5. Payload Hashing

`REMANENCE.file_sha256` is computed over the exact `Z` payload bytes —
never over tar headers, padding, or block fill. When the writer
receives payload bytes as a stream, it MUST recompute the SHA-256 of
the bytes actually consumed and MUST fail the object (refusing to
complete it) if the recomputed digest or byte count differs from the
declared spec. This proves the writer archived the payload it was
given, not the payload the metadata describes.

### 7.6. Path and Identity Rules

For every payload entry, writers MUST enforce:

1. `path` is non-empty UTF-8, contains no NUL and no byte < 0x20.
2. `path` is a **canonical relative path**: it does not begin with
   `/`, does not end with `/`, and none of its `/`-separated
   components is empty, `.`, or `..` (Appendix A.6).
3. `path` is not `_remanence` and does not start with `_remanence/`
   (the reserved namespace; the manifest is the only `_remanence/`
   entry in 1.0).
4. No two entries in one object share a `path`.
5. No two entries in one object share a `REMANENCE.file_id`; the
   manifest's `file_id` MUST also be distinct from every payload
   `file_id`.

Readers MUST reject an entry whose effective path violates rules 1–2
(`InvalidPath`): an object claiming `rem-tar-v1` with a traversal-shaped
or non-canonical path is nonconformant, and accepting it would push the
hazard onto every downstream consumer (Section 14.4). Rules 3–5 are
writer-side; Readers are not required to track cross-entry uniqueness
(Verifiers and Consumers catch duplicates via the manifest,
Section 8.3).

Within those rules, paths are byte sequences stored verbatim: the
format performs no Unicode normalization (NFC and NFD spellings of the
same name are distinct paths), no case folding, and no separator
translation. Faithful preservation of *legacy* paths — absolute paths,
`..`, arbitrary encodings — is explicitly a foreign-format (BRU)
concern, not a property of newly written native objects.

### 7.7. Entry Order

Payload entries appear in caller-supplied order; the format assigns no
meaning to the order beyond determinism. The manifest MUST be the last
entry (Section 3.1). Readers identify the manifest by its exact path
`_remanence/manifest.cbor` and MUST NOT rely on `REMANENCE.is_manifest`
alone. Readers MUST reject any entry appearing after the manifest
entry (`Parse`): such an entry cannot be listed in the manifest, so
the object's self-description would be silently incomplete.

## 8. The Object Manifest

### 8.1. Placement and Identity

The manifest is a generated regular-file entry, last in the archive,
with:

- `path` = `_remanence/manifest.cbor`
- `REMANENCE.is_manifest` = `true`
- `REMANENCE.executable` = `false`
- `REMANENCE.file_sha256` = SHA-256 of the manifest CBOR bytes
  (`manifest_sha256`)
- standard alignment, hashing, and chunk-geometry rules of Section 7.

The manifest **excludes itself**: its `file_entries` array lists
payload files only. The manifest's own identity lives in its pax header
and, externally, in the Layer 3c bootstrap row
(`manifest_first_chunk_lba`, `manifest_size_bytes`,
`manifest_chunk_count`, `manifest_sha256`), which is what enables
direct LOCATE-to-manifest reading without scanning the archive.

### 8.2. REM-CBOR: the Deterministic Encoding

The manifest payload is a single CBOR [RFC8949] data item in the
deterministic subset defined here, called REM-CBOR. (REM-CBOR is
deliberately compatible with AOF1-CBOR [AOF1] §5.1 — one decoder
serves both — but restricts the repertoire further; the differences
are noted inline.)

**Item repertoire.** A REM-CBOR item MUST be one of:

| Major type | Permitted |
| --- | --- |
| 0 | Unsigned integers 0 through 2^64 − 1 |
| 2 | Definite-length byte strings |
| 3 | Definite-length UTF-8 text strings |
| 4 | Definite-length arrays of REM-CBOR items |
| 5 | Definite-length maps with **text-string keys** and REM-CBOR values |
| 7 | Simple values `false` (20), `true` (21), and `null` (22) only |

Negative integers (major type 1), tags (major type 6), floats,
indefinite-length items, `undefined`, and all other simple values MUST
NOT appear; decoders MUST reject them with `Cbor`.

**Encoding requirements** (decoders MUST reject violations with
`Cbor`):

1. Every integer value and every length argument uses the shortest
   possible encoding (RFC 8949 preferred serialization).
2. Map keys are sorted in strictly ascending bytewise lexicographic
   order of their **deterministic encodings** (RFC 8949 §4.2.1). For
   definite-length text keys this orders by encoded length prefix
   first, then key bytes — *not* plain alphabetical order. Duplicate
   keys MUST NOT appear.
3. Text strings are valid UTF-8.
4. The manifest item occupies the entire manifest payload exactly; no
   trailing bytes.

Encoded-form key ordering is load-bearing for design goal 4: the
manifest's bytes, and therefore `manifest_sha256`, must be identical
across independent implementations. A validator MUST check canonical
form over the original encoded bytes, not by decode-and-re-encode with
a library whose defaults differ.

**Structural limits** (format-level, so every conformant decoder
accepts and rejects the same objects):

1. An object MUST NOT contain more than `MAX_FILE_ENTRIES`
   (10,000,000) payload entries, and the manifest's `file_entries`
   array is bounded accordingly.
2. Manifest nesting depth MUST NOT exceed `MANIFEST_MAX_DEPTH` (8),
   counting the top-level map as depth 1. (The 1.0 schema uses depth 4;
   the headroom is the 1.x extension budget.)
3. Decoders MUST enforce both limits incrementally during decoding and
   MUST bound allocations by the manifest's declared size (known from
   the entry's pax `size` before any CBOR byte is parsed), never by
   counts read from the CBOR stream.

### 8.3. Manifest Schema

The top-level item MUST be a map with exactly these seven text keys in
1.0 (shown here in their encoded sort order):

| Key | Type | Constraint |
| --- | --- | --- |
| `object_id` | text | MUST equal the global `REMANENCE.object_id` |
| `chunk_size` | unsigned | MUST equal the global `REMANENCE.chunk_size` |
| `file_entries` | array | One file-entry map per payload file, in archive order |
| `schema_version` | unsigned | MUST be 1 (`MANIFEST_SCHEMA_VERSION`; tracks the major version only) |
| `object_metadata` | map | Reserved; MUST be empty (`{}`) in 1.0 writers |
| `caller_object_id` | text | MUST equal the global `REMANENCE.caller_object_id` |
| `external_references` | array | Reserved; MUST be empty (`[]`) in 1.0 writers |

Each `file_entries` element is a map with exactly these eight keys in
1.0 (encoded sort order):

| Key | Type | Constraint |
| --- | --- | --- |
| `path` | text | Effective entry path |
| `file_id` | text | Entry `REMANENCE.file_id` |
| `executable` | `true`/`false`/`null` | `null` when the writer was given no value |
| `size_bytes` | unsigned | Effective payload length |
| `chunk_count` | unsigned | Section 7.4 value |
| `file_sha256` | bytes | Exactly 32 bytes; binary SHA-256 (hex in pax, binary here) |
| `first_chunk_lba` | unsigned/`null` | Section 7.4 value; `null` iff `size_bytes` = 0 |
| `metadata_preservation_data` | map | Reserved; MUST be empty (`{}`) in 1.0 writers |

Consumer obligations:

1. Before interpreting any manifest field, a Consumer MUST verify the
   manifest bytes against an anchor digest: the bootstrap/catalog
   `manifest_sha256` when available, or — self-consistency only — the
   manifest entry's own pax `REMANENCE.file_sha256`
   (`ManifestDigestMismatch` on failure). An unverified manifest is
   untrusted input from removable media.
2. A Consumer MUST reject a manifest violating the type or value
   constraints above (`ManifestInvalid`), including the cross-checks:
   `object_id`, `caller_object_id`, and `chunk_size` MUST equal the
   corresponding global header values when both are in hand.
3. A Consumer MUST treat unknown additional keys (top-level or
   per-entry) as a 1.x extension and ignore them (Section 12.2), and
   MUST NOT reject a manifest whose reserved maps/arrays are
   non-empty — that is the designated 1.x extension surface.
4. When both the manifest and the archive entries are available, a
   Consumer SHOULD verify they correspond exactly — same paths, sizes,
   hashes, and chunk geometry, with no extras on either side
   (Verifiers MUST; Section 11.4).

### 8.4. Chain of Trust

```text
3c bootstrap row (manifest location + manifest_sha256)
        │  externally anchored, parity-protected
        ▼
manifest.cbor  ── byte-verified by manifest_sha256
        │
        ▼
per-file  file_sha256, size_bytes, first_chunk_lba, chunk_count
        │
        ▼
payload bytes ── byte-verified by file_sha256
```

The pax `REMANENCE.file_sha256` keywords duplicate the manifest hashes
as a within-tape cross-check, allowing per-file verification even when
the manifest's blocks are damaged (and vice versa). What this chain
does *not* provide is covered in Section 14.1.

## 9. End of Archive

### 9.1. Tar EOF

After the manifest entry's padding, writers MUST emit exactly two
all-zero 512-byte records. Readers MUST treat an all-zero header record
followed by a second all-zero record as end of archive, and MUST reject
an all-zero record followed by a non-zero record with `Parse`
("single zero tar EOF record").

### 9.2. Final Zero Fill and Block Count

After the EOF records, writers MUST fill the remainder of the final
body block with zero bytes, so that the object's total length is

```text
total_size_bytes      = roundup(offset_after_EOF, chunk_size)
projected_size_blocks = total_size_bytes / chunk_size
```

This is the only block-level zero fill in the format, and it is
tar-safe: it lies beyond the archive EOF where standard tar already
stops. Readers MUST NOT interpret bytes after the EOF records and are
not required to verify them; Verifiers (Section 11.4) MUST confirm the
fill is all-zero and report a nonzero fill as a nonconformity — it
indicates a defective writer, damage, or a covert channel, even though
it cannot affect payload recovery. A writer whose emitted block count
differs from its planned `projected_size_blocks` MUST fail the object
rather than complete it.

## 10. Writer Obligations

### 10.1. Workflow

1. Validate options: `chunk_size` (Section 3.2) and non-empty
   `object_id`, `caller_object_id`, `write_timestamp`,
   `manifest_file_id`.
2. Validate every payload spec per Section 7.6 (paths, reserved
   namespace, duplicates).
3. Plan the full layout (Section 10.2), which also serializes the
   manifest and computes `manifest_sha256`.
4. Emit the global header, each payload entry, and the manifest entry,
   streaming payload bytes through the running SHA-256 check of
   Section 7.5.
5. Emit tar EOF and the final zero fill; verify the emitted block
   count equals the plan; report the layout (including
   `projected_size_blocks`, per-file `first_chunk_lba`, manifest
   geometry, and `manifest_sha256`) to the caller for cataloging.

A failed object MUST NOT be reported as complete; commit semantics
belong to Layer 3c (Section 1.2), and an uncommitted tape file MUST NOT
be referenced by any durable catalog.

### 10.2. Planning Determinism

The Planner computes the entire layout — every offset, pad size,
manifest byte, and the final block count — from the file *specs* alone
(path, file_id, size, hash, optional mtime/executable), without payload
bytes. Planner and Writer MUST share the same sizing rules such that
the planned layout is byte-exact; the reference implementation enforces
this by routing the writer through the planner's code and re-checking
each entry's alignment at write time. `projected_size_blocks` is the
value handed to Layer 3c's capacity reserve before `begin_object`; it
is exact, not an estimate.

### 10.3. Incomplete Block Writes

The writer consumes a block sink that reports per-block outcomes. A
block write that commits fewer bytes than the full block, or that
reports hard end-of-medium, MUST fail the object
(`IncompleteBlockWrite`); a partially-committed body block is never
valid. Early-warning state on a fully-committed block is not an error
at this layer (capacity policy is 3c's concern).

## 11. Reader Obligations

### 11.1. Inputs and Profiles

A Reader receives a block source positioned at the object's
`BodyLba(0)`, the object's `chunk_size`, and its block count. Two
profiles exist:

- **Streaming** (RECOMMENDED for restore): parses headers in order and
  delivers each payload incrementally to a sink; memory use is
  O(`chunk_size` + one pax header).
- **Materializing** (compatibility): reads the whole object into
  memory first. Implementations MUST bound the up-front allocation
  with a fallible reservation and SHOULD prefer the streaming profile;
  the block count is semi-trusted catalog data (Section 14.3).

Both profiles MUST apply identical acceptance rules.

A Reader additionally operates in one of two modes:

- **Restore mode** (the default): payload bytes are being recovered as
  content. Integrity verification is mandatory (Section 11.2 step 5).
- **Salvage mode**: a deliberately-selected, explicitly-labeled mode
  for damaged media, in which verification failures are reported but
  delivery continues. An implementation MUST NOT make salvage the
  default and MUST NOT silently fall back to it; the caller's choice
  to accept unverified bytes must be explicit (compare the dirty-state
  philosophy of Layer 2).

### 11.2. Procedure

1. Read 512-byte records from the block stream. A short block read is
   a hard error.
2. On an all-zero record: require the second EOF record (Section 9.1),
   run the Section 6.2 global checks (covers empty objects), and stop.
   Remaining blocks are ignored.
3. Verify the header checksum (Section 4.4).
4. Dispatch on typeflag (Section 4.5):
   - `g`: parse records (Section 5.1), merge into the global set
     (last-wins), defer re-validation to the next entry.
   - `x`: parse records; they attach to the next regular entry.
   - `0` / NUL: a regular entry —
     a. run the Section 6.2 global checks if not yet run for the
        current global set;
     b. compute effective path and size (Section 5.4);
     c. verify `REMANENCE.compression` (Section 7.2);
     d. if size > 0, verify chunk alignment of the payload offset
        (Section 7.3);
     e. deliver exactly `size` payload bytes, then skip the record
        padding. EOF inside a declared payload or its padding is
        `TruncatedPayload`.
   - anything else: `UnsupportedTarTypeflag`.
5. **Integrity (restore mode).** For every entry delivered in full,
   compute SHA-256 over the delivered payload bytes while streaming
   and compare against the entry's `REMANENCE.file_sha256`; on
   mismatch, fail the entry with `FileDigestMismatch` before reporting
   it restored (in salvage mode: deliver, but report the mismatch).
   This check is what makes a reported restore a verified restore;
   framing acceptance alone proves nothing about content
   (Section 14.2). Partial-range reads cannot verify a whole-file
   hash; their integrity comes from the Layer 3c block CRCs, and
   range-read implementations MUST say so rather than imply
   hash-verified content.
6. Capture the entry whose effective path is
   `_remanence/manifest.cbor` as the manifest bytes.

### 11.3. Foreign-Tolerance Summary

A conformant Reader accepts slightly-foreign archives where safe
(unsorted/duplicate pax records, NUL typeflag, `prefix`-formed names,
missing pax `path`/`size` with ustar fallback, later `g` headers) and
rejects where silence would lie (unknown typeflags, unknown
format/major, non-`none` compression, misaligned data, traversal-shaped
paths, checksum mismatch). The dividing line is: tolerate
representational variance, never tolerate uninterpretable or unsafe
content.

### 11.4. Verification Profile

A Verifier validates a complete object without extracting it — the
scrub primitive for media audits and post-write verification. A
Verifier MUST:

1. apply the full Reader procedure of Section 11.2 in restore mode,
   discarding payload bytes after hashing (every entry digest is
   therefore checked);
2. verify the manifest digest against the strongest available anchor
   (Section 8.3 rule 1) and validate the manifest schema;
3. verify the manifest-vs-archive correspondence: every payload entry
   appears in `file_entries` with matching `path`, `size_bytes`,
   `file_sha256`, `first_chunk_lba`, and `chunk_count`, and
   `file_entries` lists nothing absent from the archive;
4. verify the final-block zero fill (Section 9.2); and
5. report all nonconformities found, not only the first — a Verifier
   is a diagnostic tool, and "first error only" hides the damage
   extent that media-recovery planning needs.

A successful verification statement means: every payload byte on tape
hashes to its declared identity, and the object's self-description is
complete and consistent. It does NOT authenticate the writer
(Section 14.1).

## 12. Versioning and Extensibility

### 12.1. Version Identifiers

Two coupled identifiers version the format:

- `REMANENCE.format_id` = `rem-tar-v1` — the format's name. Any change
  that can make a conformant 1.0 Reader misinterpret bytes (new entry
  kinds with payload semantics, alignment changes, manifest
  re-keying, compression, encryption) REQUIRES a new `format_id`
  (`rem-tar-v2`); there is no in-place agility.
- `REMANENCE.schema_version` = `<major>.<minor>` — major is fixed at 1
  for `rem-tar-v1` and gates Readers (Section 6.2); minor signals
  backward-compatible additions and MUST NOT affect acceptance. The
  manifest's integer `schema_version` tracks the major only.

### 12.2. The 1.x Extension Surface

A minor revision MAY add: new `REMANENCE.*` pax keywords (global or
per-entry); new top-level or per-entry manifest keys; and content
inside the reserved `object_metadata`, `external_references`, and
`metadata_preservation_data` containers (the designated landing zone
for symlink/hardlink/directory records, xattrs, and ownership tiers).
1.0 Readers MUST tolerate all of these per Sections 5.3 and 8.3. A
minor revision MUST NOT change any rule a 1.0 Reader enforces — in
particular, since 1.0 Readers reject any `REMANENCE.encryption` or
`REMANENCE.compression` value other than `none` (the refusal gates),
introducing a real value for either is by definition a new-`format_id`
change, never a 1.x one.

### 12.3. Compression

`REMANENCE.compression` is `none` in 1.0. Format-level compression was
deliberately removed (the payload workload is already-compressed media;
whole-stream compression also destroys closed-form range addressing —
see [AOF1] §13.5 for the same argument). The keyword is retained so
that a future format (`rem-tar-v2`, per Section 12.2) can reintroduce
per-file compression behind the existing Reader gate: a 1.0 Reader
encountering any other value refuses loudly instead of restoring
garbage.

## 13. Errors

Implementations SHOULD expose typed errors equivalent to the following
taxonomy. The names are normative for the test-vector manifest
(Section 16); surface syntax is not.

```text
InvalidInput              caller-supplied object/file metadata violates Section 7.6 / 10.1
Layout                    layout arithmetic overflowed or an invariant could not be satisfied
Parse                     malformed archive structure (octal fields, EOF sequence, missing
                          required pax keys, short blocks, entry after the manifest,
                          truncated/overflowing offsets)
UstarChecksumMismatch     Section 4.4 failure
UnsupportedTarTypeflag    Section 4.5 rejection
ChunkAlignmentViolation   Section 7.3 reader rejection
ChunkSizeMismatch         archive REMANENCE.chunk_size disagrees with supplied geometry (6.2)
InvalidPath               effective path violates Section 7.6 rules 1-2 (reader-side)
TruncatedPayload          EOF inside declared payload, pax body, or padding
PaxRecordMalformed        Section 5.1 grammar violation
FileDigestMismatch        delivered payload bytes do not hash to REMANENCE.file_sha256 (11.2)
Cbor                      manifest is not valid REM-CBOR (Section 8.2)
ManifestInvalid           manifest violates the Section 8.3 schema or cross-checks
ManifestDigestMismatch    manifest bytes do not hash to the anchor digest (Section 8.3)
UnsupportedFeature        unknown format_id, schema major mismatch, non-none compression
                          or encryption
IncompleteBlockWrite      Section 10.3 writer failure
SourceIo                  payload source read failure (not a format violation)
TapeIo                    block sink/source failure (not a format violation)
```

Condition mapping (normative for single-fault test vectors):

| Condition | Error |
| --- | --- |
| Duplicate path / duplicate file_id / reserved `_remanence` path / non-canonical or control-char path / malformed `mtime` / empty required option (writer-side) | `InvalidInput` |
| Pad equation unsolvable within bound; offset/round-up overflow | `Layout` |
| `REMANENCE.format_id` ≠ `rem-tar-v1`; schema major ≠ 1; `REMANENCE.compression` ≠ `none`; `REMANENCE.encryption` ≠ `none` | `UnsupportedFeature` |
| `REMANENCE.chunk_size` present and ≠ supplied chunk_size | `ChunkSizeMismatch` |
| Missing `REMANENCE.format_id` / `REMANENCE.schema_version` / `REMANENCE.compression`; malformed octal/decimal; single zero EOF record; entry after the manifest; short body block | `Parse` |
| Header checksum mismatch | `UstarChecksumMismatch` |
| Typeflag outside {`g`,`x`,`0`,NUL} | `UnsupportedTarTypeflag` |
| size > 0 payload not chunk-aligned | `ChunkAlignmentViolation` |
| Effective path absolute / contains `.`,`..`, empty component / trailing `/` (reader-side) | `InvalidPath` |
| Pax/payload/padding bytes missing before EOF | `TruncatedPayload` |
| Pax length/separator/newline/UTF-8/control-char violations | `PaxRecordMalformed` |
| Delivered payload hash ≠ `REMANENCE.file_sha256` (restore mode) | `FileDigestMismatch` |
| Manifest bytes ≠ anchor digest | `ManifestDigestMismatch` |
| Manifest schema/type/cross-check violation; structural limits exceeded with valid CBOR | `ManifestInvalid` |
| Manifest not valid REM-CBOR (repertoire, canonical form, trailing bytes) | `Cbor` |
| Streamed payload hash or size differs from spec | `InvalidInput` (writer-side; MUST NOT complete the object) |

I/O failures MUST remain distinguishable from format violations so
callers can tell storage problems from invalid objects. No code path
reachable from archive bytes may panic, crash, or allocate unboundedly
(Section 14.3).

## 14. Security Considerations

### 14.1. No Self-Authentication

`rem-tar-v1` provides integrity *plumbing*, not authentication. An
attacker who can rewrite the tape can rewrite payloads, pax hashes, and
the manifest consistently. The trust anchor is external: the Layer 3c
bootstrap's `manifest_sha256` (itself parity-protected and
catalog-anchored) and Layer 4's audit/catalog records. A lone archive
whose hashes verify internally proves only self-consistency.
Deployments needing cryptographic authenticity get it from the payload
layer (AOF1 `aead-stream-v1` objects as payload files), not from this
format.

### 14.2. Hostile-Input Posture

Archive bytes come off removable media and MUST be treated as
untrusted. The format bounds every parse decision: pax record lengths
are validated against the remaining header payload; payload sizes are
validated against the remaining declared blocks before allocation
(streaming readers allocate O(1)); all offset arithmetic is checked.
Reader implementations MUST NOT panic or abort on any archive byte
sequence and SHOULD validate this with coverage-guided fuzzing of the
record loop, the pax parser, and the manifest decoder (Section 17).

Note the integrity coverage at this layer: the ustar header record is
checksummed, but pax bodies and payload bytes are not — a flipped bit
in a pax body changes metadata silently as far as tar framing is
concerned. The defense is layered: Layer 3c CRCs every tape block
beneath this format, and the Section 11.2/11.4 digest checks catch
metadata-vs-payload divergence above it. This is why restore-time
digest verification is a MUST and not an optimization.

### 14.3. Semi-Trusted Catalog Inputs

`chunk_size` and the block count arrive from the catalog/bootstrap, not
from the archive. A corrupted catalog can therefore request absurd
allocations from a materializing Reader. Materializing Readers MUST use
fallible allocation and SHOULD enforce a deployment-level size ceiling;
streaming Readers are immune by construction and are the production
path.

### 14.4. Path Traversal

Defense is layered. At the format level, native objects cannot
represent traversal: Section 7.6 forbids absolute paths and `.`/`..`/
empty components at write time, and Readers reject violations
(`InvalidPath`), so a conformant `rem-tar-v1` path is always a clean
relative path. Restore implementations MUST nevertheless keep their
own sanitization — refuse to follow symlinks in the destination tree,
open with `O_NOFOLLOW` or equivalent, re-check components — because
restore sinks also serve foreign formats (BRU archives carry absolute
and `..` paths), salvage-mode reads, and standard-tool extractions
that bypass this Reader entirely. In Remanence this is
`remanence-stream`'s restore-sink contract; any independent
implementation inherits the same obligation. Framing-layer acceptance
of a path is a necessary check, not a sufficient safety claim.

### 14.5. UTF-8 Strictness

All text is strict UTF-8, enforced at write time; the writer MUST
refuse (never transliterate) a non-UTF-8 path. Rationale: mixed-encoding
filenames in predecessor deployments were silently transliterated at
catalog boundaries, after which restore lookups no longer matched the
on-tape names. Eliminating the encoding boundary at write time is the
fix; readers can then trust that pax `path`, manifest `path`, and
catalog paths are the same byte sequences.

## 15. Interoperability and the 30-Year Fallback

### 15.1. Standard-Tool Extraction

The archive body is a valid pax archive. With GNU tar, bsdtar, or any
pax-aware reader:

```sh
mt -f /dev/nst0 fsf <n>            # position to the object's tape file
tar -b <chunk_size/512> -xf /dev/nst0    # e.g. -b 512 for 256 KiB blocks
```

extracts every payload file byte-correct, plus one extra file
`_remanence/manifest.cbor`. Unknown `REMANENCE.*` keywords are ignored
by POSIX rule; `REMANENCE.pad` inflates only header size, never
content. The manifest decodes with any generic CBOR tool into
self-describingly-named fields — this is why the manifest uses text
keys (Appendix A.1).

### 15.2. Forward-Scan Fallback

With all Remanence metadata lost, a Scanner can walk the archive using
only tar rules — header, `size`, `roundup512(size)`, repeat — because
the format never violates tar framing. Filemark-delimited tape files
make per-object isolation trivial (`mt fsf`). This degraded path
recovers all payload bytes and names; it loses only chunk addressing
(irrelevant when scanning) and verification (recoverable from the
manifest if its blocks survive).

### 15.3. Interop Gates

Conformance requires demonstrated extraction equality (byte-identical
payloads, matching paths) by **GNU tar**, **bsdtar**, and **Python
`tarfile`** for the positive test vectors of Section 16, including the
boundary-size and placeholder-path cases. (These three independent
implementations are the proxy for "any future pax reader".)

## 16. Test Vector Requirements

Static vectors live in the repository (`fixtures/rem-tar-v1/`), each
with a manifest entry recording inputs, expected layout, and — for
negative vectors — the expected Section 13 error name. Vectors use
small `chunk_size` values (e.g. 4096) so full object byte streams are
practical to pin; at least one vector MUST use `DEFAULT_CHUNK_SIZE`.

### 16.1. Positive Vectors

The suite MUST include at least:

1. **Empty object** — zero payload files (global header + manifest +
   EOF only).
2. **Empty file** — one zero-length payload (`chunk_count` 0, absent
   `first_chunk_lba`, `null` in the manifest).
3. **One-byte file.**
4. **Block-boundary set** — payload sizes `chunk_size − 1`,
   `chunk_size`, `chunk_size + 1`, and one multi-chunk size.
5. **Pathological paths** — a non-ASCII path and a > 100-byte path
   (both exercising `PAX_PATH_PLACEHOLDER`), and a 100-byte portable
   path stored inline.
6. **Full metadata** — entries with `mtime`, `executable=true`
   (mode 0755), and `executable` unsupplied (`null` in manifest).
7. **Multi-file object** ordering payload entries non-alphabetically
   (pinning caller-order preservation).
8. **Canonical-manifest byte-identity vector** — pins the exact
   manifest CBOR bytes and `manifest_sha256` for a fixed input set.
   This is the cross-implementation determinism gate (Section 8.2).

For each positive vector the manifest MUST pin: the exact full object
byte stream (or its SHA-256 plus the first object block verbatim, for
large vectors), `projected_size_blocks`, every entry's
`(pax_header_offset, data_offset, first_chunk_lba, chunk_count,
pad_spaces)`, the manifest CBOR bytes, and `manifest_sha256`.

### 16.2. Negative Vectors

Each contains exactly one fault and asserts the mapped error:

Writer-side (constructed via API, not bytes): duplicate path; duplicate
`file_id`; manifest `file_id` colliding with a payload `file_id`;
reserved `_remanence/` path; control character in path; each
non-canonical path shape (`/abs`, `a/../b`, `./a`, `a//b`, `a/`);
malformed `mtime` (non-decimal); streamed payload with wrong hash;
streamed payload with wrong size; non-multiple-of-512 `chunk_size`.

Reader-side (byte vectors): wrong `REMANENCE.format_id`; schema major
2; missing `REMANENCE.compression`; `REMANENCE.compression=gzip`;
`REMANENCE.encryption=aes-256-gcm`; declared `REMANENCE.chunk_size`
disagreeing with the supplied geometry; corrupted header checksum;
single zero EOF record; unknown typeflag (e.g. `5`); misaligned
nonzero payload (hand-assembled); traversal-shaped effective path
(hand-assembled `../escape`); an entry placed after the manifest
entry; one flipped payload bit (framing intact, restore MUST fail with
`FileDigestMismatch`); truncated payload; truncated pax body; pax
record with length out of bounds; pax record missing `=`; pax record
missing trailing newline; pax value with control character; non-UTF-8
pax value; non-UTF-8 octal field.

Manifest vectors (decoded by Consumers): non-canonical key order;
non-shortest integer encoding; indefinite-length item; float; tag;
duplicate map key; `schema_version` 2; `file_sha256` of wrong length;
nesting depth exceeding `MANIFEST_MAX_DEPTH`; manifest bytes
disagreeing with the anchor digest; manifest `chunk_size` disagreeing
with the global header; unknown extra key (MUST be *accepted*).

## 17. Candidate Freeze Criteria

`rem-tar-v1` remains a candidate until all of the following hold, after
which no normative change is permitted other than errata that do not
change the set of valid objects (all else requires `rem-tar-v2`):

1. The reference implementation implements this document, and every
   Appendix B gap is closed or explicitly re-specified.
2. Static vectors covering every Section 16 case exist in-repo and the
   reference implementation passes them, including the
   manifest byte-identity vector.
3. The Section 15.3 interop gates pass (GNU tar, bsdtar, Python
   `tarfile`) on all positive vectors.
4. Coverage-guided fuzzing of the record loop, pax parser, and
   manifest decoder reaches a corpus plateau with no panics, crashes,
   hangs, or unbounded allocations.
5. A live round-trip passes on the QuadStor VTL and on the MSL3040
   (write via the reference Writer through Layer 3c, read back via the
   streaming Reader, extract via standard `tar -b`), at two distinct
   `chunk_size` values.
6. An independent exercise confirms a payload file can be located and
   verified using only this document, a generic CBOR tool, and
   standard `tar` (the 30-year drill).

## 18. References

### 18.1. Normative

- [RFC2119] / [RFC8174] — BCP 14 requirement keywords.
- [RFC8949] — Concise Binary Object Representation (CBOR), STD 94.
- [POSIX-PAX] — IEEE Std 1003.1, `pax` Interchange Format (ustar and
  pax extended headers).

### 18.2. Informative

- [AOF1] — Archive Object Format 1, Candidate Specification
  (`~/amber/docs/aof1_candidate_specification.md`): the payload-layer
  container; REM-CBOR's relationship to AOF1-CBOR.
- `docs/spec-v0.4.md` §8 — architectural context, address spaces,
  Layer 3b/3c boundary, design history.
- `docs/rem-tar-v1-design.md` v0.9.3 — superseded design document
  (Appendix A).
- `docs/pfr-reference.md` — partial-file-restore mechanics and worked
  range examples.
- `docs/tape-block-size-config-design-v0.1.md` — how `chunk_size` is
  chosen.

---

## Appendix A. Decisions and Superseded Material

This appendix records wire-format decisions where prior documents
disagreed or were silent, so future revisions do not silently reverse
them.

### A.1. Manifest schema: text keys, not integer keys

`rem-tar-v1-design.md` v0.9.3 §8.1 specified an integer-keyed manifest
with a `ManifestHeader`, packed per-chunk CRC-64 arrays (`chunk_crcs`),
`mtime`, and a `compression` field. **That schema was never
implemented and is superseded** by the text-keyed schema of Section 8.3
(which `spec-v0.4.md` §8.7.5 already reflected). Rationale: text keys
keep the manifest self-describing to a generic CBOR tool with no schema
document in hand — directly serving the 30-year goal — at a size cost
that is negligible against media payloads. Specifically dropped, with
reasons:

- **Per-chunk CRC-64s**: block-level integrity belongs to Layer 3c
  (sidecar CRC-64 per tape block); duplicating it per-chunk in the
  manifest added size and a second source of truth. Whole-file SHA-256
  remains.
- **Manifest `mtime`**: carried in pax only (Section 5.5); a 1.x
  revision can add it to `metadata_preservation_data` if needed.
- **Manifest `compression`**: redundant with the per-entry pax gate.

### A.2. Canonical CBOR: RFC 8949 encoded-form key order

Earlier documents said "canonical CBOR / sorted keys" without defining
the comparison. Plain alphabetical key order and RFC 8949 §4.2
encoded-form order genuinely differ for these keys (length-first:
`object_id` < `chunk_size` < `file_entries` < ...). **Resolved: RFC
8949 §4.2 deterministic encoding, encoded-form order**, aligning
REM-CBOR with AOF1-CBOR so one deterministic-CBOR implementation
serves the whole ecosystem. The byte-identity vector (Section 16.1
item 8) pins it.

### A.3. Uniform alignment, including zero-length entries

The alignment invariant applies to every entry, not just entries with
payload. A zero-length entry's pad buys no read-side capability, but a
single unconditional rule keeps Planner/Writer layout identical and
removes a conditional from every independent implementation.

### A.4. Regular-files-only scope for 1.0

`spec-v0.4.md` §8.7.7's symlink classification, directory entries, and
hardlink design are intended semantics for a 1.x minor revision via the
reserved manifest surfaces, not part of the 1.0 wire format. Readers
reject unknown typeflags rather than skip them (Section 4.5) precisely
so a 1.x archive is never silently half-restored by a 1.0 reader —
if 1.x link entries are ever encoded as new *typeflags* rather than
manifest records, that is by definition a `rem-tar-v2` change.

### A.5. `REMANENCE.encryption` is a refusal gate, not a feature

The body format will never encrypt (payload-layer AOF1 owns
confidentiality). The keyword exists so that *if* a future revision
ever marks an object's payloads as non-plaintext, 1.0 readers refuse
loudly instead of restoring ciphertext as content — which is why
checking it is a Reader MUST (Section 6.2), not advisory.

### A.6. Canonical relative paths are a format rule, not a restore nicety

Earlier drafts (and the current implementation) stored paths verbatim,
deferring all traversal defense to restore-time sanitization. Resolved:
native objects MUST carry canonical relative paths (Section 7.6), and
readers reject violations. Rationale: `rem-tar-v1` writes *new*
archives from staged trees — there is no legitimate input whose path
needs `..`, a leading `/`, or empty components; representational
faithfulness to messy legacy paths is exactly the foreign-format
(BRU) reader's job, and conflating the two surrendered a free, zero-cost
structural defense. Restore sanitization remains mandatory as
defense-in-depth (Section 14.4), because restore sinks also serve
foreign formats and salvage reads.

### A.7. Restore-time digest verification is mandatory

Earlier drafts let framing acceptance stand in for integrity, with hash
checking left to optional consumers — the precise gap the 2026-06-10
implementation review flagged (restores reporting success on silently
corrupted payloads). Resolved: a Reader in restore mode MUST verify
`REMANENCE.file_sha256` for every fully delivered entry
(Section 11.2 step 5), and unverified delivery exists only behind an
explicit salvage mode. The hash is computed over bytes that already
stream through the reader, so the cost is one SHA-256 pass that the
format's whole trust chain (Section 8.4) presumes is happening.

## Appendix B. Reference Implementation Gaps (as of 2026-06-10)

Requirements of this document not yet met by `crates/remanence-format`
(and its consumers) at this revision's date. This is the conformance
backlog: the specification leads, and each item must be closed in code
— or the requirement consciously amended here — before freeze.
Section references give the governing requirement.

Reader/Verifier:

1. **Restore-time digest verification (§11.2 step 5, A.7)** — the
   streaming and materializing readers deliver payloads without
   checking `REMANENCE.file_sha256`; no salvage/restore mode split
   exists. The highest-priority gap (review finding H12).
2. **Verifier profile (§11.4)** — `archive verify` exists as a CLI
   surface but no component implements the five Verifier obligations
   (all-digests + manifest anchor + correspondence + zero-fill +
   report-all).
3. **`REMANENCE.encryption` gate (§6.2 item 3)** — unchecked.
4. **`REMANENCE.chunk_size` cross-check (§6.2 item 4)** — unchecked;
   `ChunkSizeMismatch` does not exist as an error.
5. **Canonical-path rejection (§7.6, `InvalidPath`)** — readers accept
   any effective path without shape validation.
6. **Entry-after-manifest rejection (§7.7)** — readers accept
   trailing entries.
7. **`REMANENCE.chunk_count` consistency check (§7.2, SHOULD)** — not
   performed.

Writer/Planner:

8. **Canonical-path enforcement (§7.6 rule 2)** — the planner blocks
   NUL/control characters and the reserved prefix but not `..`,
   absolute paths, empty components, or trailing `/`.
9. **Manifest `file_id` uniqueness vs payload ids (§7.6 rule 5)** —
   unchecked.
10. **`mtime` shape validation (§5.4)** — recorded verbatim,
    unvalidated.
11. **`MAX_FILE_ENTRIES` enforcement (§8.2)** — the design's
    10M-entry pre-write cap is not enforced at the format layer.

Manifest/Consumer:

12. **No manifest validator** — Section 8.3 Consumer obligations
    (anchor-digest verification, schema validation, canonical-form
    check over encoded bytes, structural limits) have no reference
    implementation; manifest bytes are produced and transported but
    never validated on read. `ManifestInvalid` /
    `ManifestDigestMismatch` do not exist as errors.

Conformance machinery:

13. **Test vectors absent (§16)** — `fixtures/rem-tar-v1/` does not
    exist; the §15.3 interop gate currently covers Python `tarfile`
    only, via unit test rather than static vectors (GNU tar and bsdtar
    gates missing).
14. **No fuzz targets (§17 criterion 4)** — not started.
15. **Uncommitted baseline** — the wire behavior this document
    describes (RFC 8949 manifest order, format gates,
    duplicate/reserved-path validation, `IncompleteBlockWrite`) is in
    the 2026-06-10 working tree but not yet committed to `main`.

## Appendix C. Worked Alignment Example

`chunk_size` = 4096. First payload file of an object: the global header
occupies records 0–1 (one ustar record + one 512-byte-padded pax body),
so the entry's pax ustar record begins at `O` = 1024.

The alignment equation requires
`1024 + 512 + R + 512 ≡ 0 (mod 4096)`, i.e. `R ≡ 2048 (mod 4096)`.

If the entry's base pax records (path, size, REMANENCE.* keys) encode
to 612 bytes, the minimum pad record (`"18 REMANENCE.pad=\n"`, zero
spaces, 18 bytes — note `18` counts its own two digits) gives a
630-byte payload, so `Rmin` = 1024 — wrong residue. The smallest
conforming target is `R` = 2048. The largest pad keeping the payload
≤ 2048 is 1416 spaces: the pad record is then
`"1436 REMANENCE.pad="` + 1416 spaces + `"\n"` (1436 = 4 length digits
+ 1 space + 13 keyword + 1 equals + 1416 + 1 newline, and 1436 has
four digits — the fixed point holds). The pax payload is 612 + 1436 =
2048 bytes exactly, rounding to `R` = 2048 with no record padding; the
file's ustar header ends at 1024 + 512 + 2048 + 512 = 4096 — the
payload starts exactly at `BodyLba(1)`. A pax-aware tar reader sees an
ordinary, slightly verbose extended header; `tar -t` output is
unchanged.
