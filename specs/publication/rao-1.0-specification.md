# Rem Archive Object (RAO) Format

> **SUPERSEDED (2026-07-11).** This split draft has been folded, together with
> the RAO 1.1 xattr delta and the on-tape v2 HPKE envelope, into the single
> combined publication document
> [`rao-object-format-1.0.md`](rao-object-format-1.0.md) — the normative RAO
> Format Specification, Version 1.0. This file is retained as an internal
> revision record only; do not cite or publish it.

## Version 1.0 — Specification

| | |
| --- | --- |
| Status | Draft for review |
| Version | 1.0 |
| Date | 2026-06-11 |
| Envelope magic | `RAO1` (encrypted representation) |
| Stream format identifier | `rao-v1` (plaintext representation) |
| Default file extension | `.rao` |

## Status of This Document

This document is a draft specification, published for review. It is the
normative fixed point for the format it defines: an implementation is
validated against this document, not the reverse. The cryptographic values in
the Section 13 test vectors that cannot be derived by arithmetic alone are
marked *pinned-at-generation*; producing them and freezing them against an
independent re-derivation is a criterion for declaring this specification
final (Section 14). Items still open before that point are collected in
Appendix C. After freeze, no normative change is permitted other than errata
that do not change the set of valid objects; any other change produces a new
major version (Section 10).

## Abstract

This document specifies the Rem Archive Object (RAO) format, version 1.0: a
backend-independent byte format for large archival objects. An RAO object
bundles many named file payloads into one self-describing unit — a constrained
POSIX pax tar stream carrying per-file SHA-256 identities, closed-form
byte-range addressing, and a deterministic CBOR manifest — and exists in
exactly two representations: **plaintext**, the bare container stream,
extractable by any standard `tar`; and **encrypted**, the same byte stream
sealed inside a confidential authenticated envelope using HKDF-SHA-256 key
derivation and a chunked ChaCha20-Poly1305 stream construction. Both
representations of one object share a logical identity (`plaintext_digest`);
each stored copy has a physical identity (`stored_digest`) that backends scrub
without keys. Encryption preserves partial file restore:
authenticated-encryption (AEAD) chunks coincide
one-to-one with the object's body blocks, so a per-file block index addresses
ciphertext by closed-form arithmetic. The format is designed for single-pass
writing, byte-stable fanout to tape, disk, and object storage, parity
protection over stored bytes, and long-term recovery from this document and
its static test vectors alone.

## Table of Contents

1. [Introduction](#1-introduction)
2. [Conventions and Terminology](#2-conventions-and-terminology)
3. [Object Model](#3-object-model)
4. [Plaintext Representation](#4-plaintext-representation)
5. [Encrypted Representation](#5-encrypted-representation)
6. [Partial File Restore](#6-partial-file-restore)
7. [Digests, Integrity, and the Verification Chain](#7-digests-integrity-and-the-verification-chain)
8. [Storage Bindings and Backend Independence](#8-storage-bindings-and-backend-independence)
9. [Relationship to the Parity Layer](#9-relationship-to-the-parity-layer)
10. [Versioning and Extensibility](#10-versioning-and-extensibility)
11. [Errors](#11-errors)
12. [Security Considerations](#12-security-considerations)
13. [Test Vectors](#13-test-vectors)
14. [Conformance and Freeze Criteria](#14-conformance-and-freeze-criteria)
15. [IANA Considerations](#15-iana-considerations)
16. [References](#16-references)

Appendix A. [Worked Example (Informative)](#appendix-a-worked-example-informative)
Appendix B. [Design Rationale (Informative)](#appendix-b-design-rationale-informative)
Appendix C. [Open Items Before Freeze (Informative)](#appendix-c-open-items-before-freeze-informative)

---

## 1. Introduction

### 1.1. Purpose and Design Goals

RAO wraps a set of named file payloads into one archival object. Its design
goals, in priority order:

1. **Plaintext longevity comes first.** A plaintext RAO object is a
   fully valid POSIX pax tar archive. A standard pax-aware `tar` extracts
   every payload byte-correct with no Remanence software present.
2. **Self-description.** Every object carries its own per-file index (the
   CBOR manifest): paths, sizes, SHA-256 identities, and the block address of
   every file inside the object. A catalog can be rebuilt from the medium —
   in the clear for plaintext objects, with the key for encrypted ones.
3. **Closed-form byte-range addressing (partial file restore, PFR).** Any byte range of any member
   file maps to stored byte ranges by arithmetic alone, in **both**
   representations. No scanning, no decompression, no whole-object read.
4. **Confidential encryption as a mode, not a fork.** The encrypted
   representation seals the *identical* plaintext byte stream — manifest
   included — inside an authenticated envelope. Nothing about the object's
   contents, filenames, or structure is visible without the key; a small
   plaintext header carries only what key recovery and keyless scrubbing
   need.
5. **Separation of identities.** The logical plaintext identity
   (`plaintext_digest`) is distinct from the physical stored identity
   (`stored_digest`); backends scrub by the latter without keys.
6. **Deterministic byte representation.** Given identical inputs and options,
   every conformant writer produces the identical byte stream, in both
   representations (the encrypted representation is deterministic given the
   key material).
7. **Long-term recoverability.** The format is recoverable from this document
   plus its static test vectors, and — degraded, plaintext representation
   only — from knowledge of POSIX tar alone.

### 1.2. Two Representations of One Object

An RAO object has a single logical form — a self-describing, bundled,
chunk-aligned pax tar stream (Section 4) — stored in either of two
representations:

- **plaintext**: the bare stream, byte-for-byte; self-describing in the
  clear and extractable by commodity `tar`.
- **encrypted**: that identical stream sealed inside a confidential,
  authenticated envelope (Section 5).

Both representations of one object wrap the identical canonical bytes and
therefore share one logical identity (`plaintext_digest`, Section 3.3). The
encrypted representation adds confidentiality and cryptographic
authentication while preserving the self-description and closed-form
byte-range addressing of the plaintext form — *with the key*.

### 1.3. Deployment Context (Informative)

In the intended deployment each archival master is kept as three copies with
different jobs:

| Copy | Role | Format |
| --- | --- | --- |
| copy-1 | working (random-access restore) | RAO, plaintext representation |
| copy-2 | offsite/disaster-recovery + cloud blob | RAO, encrypted representation |
| copy-3 | shelf (cold, last resort) | plain GNU tar — **not** RAO, by design |

Copy-3 is a deliberately different format and implementation, for
format/implementation diversity (a latent bug in the RAO writer cannot
corrupt all three baskets identically); it is out of scope of this document.
Copies 1 and 2 are the same RAO object in its two representations: built once,
fanned out byte-stable, sharing one `plaintext_digest`.

### 1.4. Relationship to Adjacent Components

RAO is the archival object format of the Remanence tape stack. It owns one
thing — the stored bytes of one object — and leaves the rest to the
components around it:

- **RAO owns** the stored bytes of one object: tar framing, alignment, vendor
  keywords, the manifest, and the encryption envelope.
- **The parity layer** [REMPARITY] owns everything outside the object's
  stored bytes on tape: the tape filemark terminating the object's tape file,
  parity sidecars, block-level CRCs, the beginning-of-tape (BOT) bootstrap,
  and the durable commit barrier. Parity is computed over **stored** bytes —
  ciphertext when the object is encrypted (Section 9).
- **The catalog and restore orchestration** above the format own catalogs,
  object selection, restore policy, and restore-time path sanitization.
- **The key registry is external** (Section 5.3). Objects carry only a
  16-byte `key_id`; the format never holds key material.

### 1.5. Non-Goals

RAO performs no compression: the payload workload is already-compressed media,
and whole-stream compression destroys closed-form range addressing (a later
member's offset would depend on decompressing earlier bytes). It defines no
catalog format, no key registry, no network protocol, and no multi-object
container: one object is one archive is one stored byte string. Version 1.0
encodes a faithful tree of files — regular files, hardlinks, symbolic links,
and (empty) directories. Device nodes, FIFOs, and sockets are excluded on
principle: they carry no content (they are kernel/runtime handles) and
materializing them on restore is a hazard, so a conformant reader rejects
their typeflags (Section 4.3.4). Ownership is deliberately not preserved
(Section 4.3.1); extended attributes are a 1.x extension surface (Section 10).
Re-keying an encrypted object without rewriting its payload bytes is not
supported (Section 12.8).

## 2. Conventions and Terminology

### 2.1. Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174]
when, and only when, they appear in all capitals, as shown here.

### 2.2. Conformance Roles

A single implementation may fill several roles.

- **Writer**: produces RAO objects. Comprises the **Builder** (produces the
  canonical plaintext stream; Section 4.9) and the **Sealer** (produces the
  encrypted representation; Section 5.8).
- **Planner**: computes a plaintext object's exact layout and block count
  without payload bytes (Section 4.9). Planning determinism extends to the
  envelope: the encrypted stored size is a closed form of the plaintext size
  (Section 5.7).
- **Reader**: recovers entries from an object in either representation
  (Sections 4.9, 5.9).
- **Verifier**: validates a complete object end to end, with key material
  when the object is encrypted (Section 7.4).
- **Keyless Verifier**: validates the public structure of an encrypted object
  and computes `stored_digest` without key material (Section 5.10).
- **Restorer**: maps payload byte ranges to stored byte ranges for partial
  file restore, in either representation (Section 6).
- **Scanner**: walks a *plaintext* object using only POSIX tar knowledge —
  the degraded long-term fallback (Section 4.10). Scanners are not required
  to implement this document. No scanner role exists for the encrypted
  representation.
- **Consumer**: interprets a decoded manifest (Section 4.7). A **Restoring
  Consumer** additionally materializes entries to a target filesystem; its
  safety obligations are stated in Section 12.10.

### 2.3. Definitions

- **Object**: one RAO archive; the unit of write, commit, replication, and
  restore.
- **Canonical plaintext object / inner stream**: the complete plaintext
  representation byte string of an object (Section 3.1). The encrypted
  representation's AEAD payload is exactly this byte string.
- **Representation**: one of `plaintext` or `encrypted` (Section 3.2). Two
  stored copies of one object may use different representations.
- **Body block / chunk**: a fixed-size block of `chunk_size` bytes; the unit
  of I/O, alignment, addressing, and (in the encrypted representation) AEAD
  chunking. "Chunk" is used for addressing, "block" for I/O; they are
  synonyms.
- **`chunk_size` (C)**: the per-object body-block size. A positive multiple
  of 512; default 262144 (256 KiB). One value per object, shared by both
  representations of that object.
- **Record**: a 512-byte tar record. All tar structures are sequences of
  records.
- **Entry**: one pax extended header + one ustar header (typeflag `0`, `1`,
  `2`, or `5`) + record padding, describing one member; only a regular entry
  (`0`) has a payload (Section 4.6).
- **Inner `BodyLba`**: zero-based index of a body block *within the canonical
  plaintext object*. This is the address space of the manifest and of all
  catalog per-file rows, and it is identical across both representations of
  one object.
- **Stored `BodyLba`**: zero-based index of a `chunk_size` block within the
  *stored* bytes of one copy. For a plaintext copy, stored `BodyLba` equals
  inner `BodyLba`. For an encrypted copy the two spaces differ; Section 6.4
  defines the mapping.
- **Stored bytes**: the exact bytes of one stored copy, from byte 0 through
  the final byte of its final block. `stored_digest` is defined over these.
- **Envelope**: the encrypted representation's framing — plaintext header,
  metadata frame, payload frame, footer, and final fill (Section 5.1).
- **Key registry**: the external system mapping a 16-byte `key_id` to root
  key material or a retrieval procedure.
- **Deterministic CBOR**: the canonical CBOR encoding rules of Section 4.7.1,
  used in two profiles — the **manifest profile** (text keys; Section 4.7)
  and the **metadata profile** (unsigned-integer keys; Section 5.5.1).

### 2.4. Integer, Byte, and Text Conventions

All fixed-width integers in the envelope header are unsigned and encoded
big-endian (network byte order). Byte offsets are zero-based. `KiB` = 2^10
bytes; `MiB` = 2^20 bytes. Hexadecimal values are prefixed `0x`. ustar numeric
fields are ASCII octal (Section 4.3). All other text in the format — pax
keywords and values, paths, manifest and metadata text strings — is UTF-8
[RFC3629]; pax keywords are additionally restricted to ASCII. SHA-256 is the
hash function of [FIPS180-4]. All derived quantities
(offsets, frame lengths, chunk counts, block counts) are defined over unsigned
64-bit arithmetic; implementations MUST use checked arithmetic and MUST NOT
wrap silently (Section 11). AEAD denotes authenticated encryption with
associated data; AAD denotes an AEAD's associated data; PRF denotes a
pseudorandom function; PFR denotes partial file restore (Section 6).

### 2.5. Constants

| Constant | Value | Meaning |
| --- | --- | --- |
| `TAR_RECORD_SIZE` | 512 | POSIX tar record size in bytes |
| `DEFAULT_CHUNK_SIZE` | 262144 (256 KiB) | Default body-block size |
| `STREAM_FORMAT_ID` | `rao-v1` | Value of the global `REMANENCE.format_id` keyword |
| `STREAM_SCHEMA_VERSION` | `1.0` | Value of the global `REMANENCE.schema_version` keyword |
| `MANIFEST_PATH` | `_remanence/manifest.cbor` | Manifest entry path |
| `RESERVED_PREFIX` | `_remanence` | Reserved path namespace (Section 4.6) |
| `USTAR_SIZE_MAX` | 0o77777777777 (8 GiB − 1) | Largest size representable in the ustar size field |
| `PAX_PATH_PLACEHOLDER` | `remanence/pax-path` | ustar name placeholder for pax-backed paths (Section 4.3) |
| `PAX_LINK_PLACEHOLDER` | `remanence/pax-linkpath` | ustar linkname placeholder for pax-backed link targets (Section 4.6.1) |
| `GLOBAL_HEADER_NAME` | `GlobalHead.0/PaxHeaders/remanence` | ustar name of the global pax header |
| `PAX_HEADER_NAME` | `PaxHeaders.0/remanence_file` | ustar name of member-entry pax headers |
| `MANIFEST_PAX_HEADER_NAME` | `PaxHeaders.0/_remanence_manifest` | ustar name of the manifest's pax header |
| `MANIFEST_SCHEMA_VERSION` | 1 | Manifest CBOR `schema_version` integer (Section 4.7) |
| `MAX_FILE_ENTRIES` | 10000000 | Maximum member entries per object (Section 4.7) |
| `MANIFEST_MAX_DEPTH` | 8 | Maximum manifest CBOR nesting depth (Section 4.7) |
| `RAO_MAGIC` | `RAO1` (`0x52 0x41 0x4F 0x31`) | Envelope magic |
| `RAO_HEADER_LEN` | 128 | Envelope plaintext header length in bytes |
| `RAO_FORMAT_VERSION` | 1 | Envelope `format_version` field value |
| `RAO_SUITE_HKDF_CHACHA` | `0x01` | `suite_id`: HKDF-SHA-256 + ChaCha20-Poly1305 |
| `RAO_KEY_ID_LEN` | 16 | Key identifier length in bytes |
| `RAO_SALT_LEN` | 16 | `hkdf_salt` length in bytes |
| `RAO_OBJECT_ID_FIELD_LEN` | 64 | Fixed `object_id` header field length in bytes |
| `RAO_TAG_LEN` | 16 | Poly1305 tag length in bytes |
| `RAO_NONCE_LEN` | 12 | ChaCha20-Poly1305 nonce length in bytes |
| `RAO_KEY_LEN` | 32 | Derived AEAD key length in bytes |
| `RAO_MAX_METADATA_FRAME_LEN` | 16777216 (16 MiB) | Maximum envelope metadata frame length |
| `RAO_MAX_CBOR_NESTING_DEPTH` | 32 | Maximum envelope metadata nesting depth |
| `RAO_MAX_METADATA_ITEMS` | 65536 | Maximum envelope metadata data-item count |
| `RAO_FOOTER` | `RAO1_STREAM_END.` | 16-byte completion footer (Section 5.7); hex `52 41 4F 31 5F 53 54 52 45 41 4D 5F 45 4E 44 2E` |
| `LABEL_SALT` | `rao1-salt-v1` | HKDF info label, 12 ASCII bytes, no terminator (Section 5.4.1) |
| `LABEL_OBJECT` | `rao1-object-v1` | HKDF info label, 14 ASCII bytes, no terminator |
| `LABEL_METADATA` | `rao1-metadata-v1` | HKDF info label, 16 ASCII bytes, no terminator |
| `LABEL_PAYLOAD` | `rao1-payload-v1` | HKDF info label, 15 ASCII bytes, no terminator |

## 3. Object Model

### 3.1. The Canonical Plaintext Object

Every RAO object has exactly one **canonical plaintext form**: the complete
stream byte string defined in Section 4 — global pax header, aligned payload
entries, manifest entry, tar EOF, and final zero fill. Its length is always a
positive exact multiple of `chunk_size` (Section 4.8). All logical properties
of the object — its member files, their identities, the manifest, the
per-file inner `BodyLba` index — are properties of this byte string and are
therefore identical across representations.

### 3.2. Representations

| Representation | Stored bytes | Confidentiality |
| --- | --- | --- |
| `plaintext` | The canonical plaintext object, verbatim | None; self-describing in the clear; `tar`-extractable |
| `encrypted` | The Section 5 envelope: plaintext header ‖ encrypted metadata frame ‖ chunked AEAD ciphertext of the canonical plaintext object ‖ footer ‖ zero fill | Confidential and authenticated; self-describing **with the key**; opaque without it |

There is no third representation: the plaintext representation is the bare
container stream itself, preserving standard-`tar` extractability, and the
encrypted representation is that same stream sealed. A writer producing both
copies of one object MUST derive them from the same canonical plaintext byte
string ("build once, fan out"), which is what makes the shared identities of
Section 3.3 hold.

### 3.3. Identities and Digests

| Digest | Computed over | Stored where | Verifiable without keys? |
| --- | --- | --- | --- |
| `file_sha256` | One regular member file's exact payload bytes | Entry pax header + manifest | Plaintext copies: yes. Encrypted copies: no |
| `manifest_sha256` | The manifest entry's CBOR bytes | Manifest pax header; for plaintext copies also the parity-layer bootstrap and catalog (Section 8.2) | Plaintext copies: yes. Encrypted copies: no |
| `plaintext_digest` | The **complete canonical plaintext object** bytes | Encrypted copies: inside the authenticated metadata frame (Section 5.5). All copies: catalog | No (for encrypted copies) |
| `stored_digest` | The **complete stored bytes** of one copy, byte 0 through the final fill byte | External only: catalog / master index (never in-band) | **Yes** — the keyless scrub anchor |

Consequences, all normative:

1. For a plaintext copy, `stored_digest` = `plaintext_digest`. The two names
   denote the same value; a catalog can store it once.
2. A plaintext copy and an encrypted copy of the same object share
   `plaintext_digest` and differ in `stored_digest`. An external index joins
   copies of one logical object by `plaintext_digest`.
3. `plaintext_digest` is a function of the canonical bytes, which include the
   global header's `object_id` and `write_timestamp` keywords and the final
   zero fill. Copies share it if and only if they wrap the identical
   canonical byte string (Section 3.2). Re-*building* an object from the same input files
   with a new `object_id`, timestamp, or `chunk_size` produces a new object
   with a new `plaintext_digest`; per-file `file_sha256` values are what
   survive across rebuilds.
4. Any stored copy MUST be scrubbable by `stored_digest` alone — no keys,
   no plaintext access, and no format knowledge beyond "a byte string" are
   required of the backend.

### 3.4. Representation Detection

For a whole-object input of unknown representation, a Reader MUST decide as
follows, examining the first bytes:

1. Bytes 0–3 equal `RAO_MAGIC` (`RAO1`) → encrypted representation
   (Section 5).
2. Otherwise → attempt the plaintext representation: the input must begin
   with a valid ustar header record, typeflag `g` (Section 4.5), and its
   global header must pass the `REMANENCE.format_id = rao-v1` gate
   (Section 4.5.2). A conformant plaintext object's first record is the
   global pax header, whose ustar name (`GlobalHead.0/PaxHeaders/remanence`)
   cannot collide with the magic.

This rule is for self-identification and tooling convenience; a deployment's
catalog records each copy's representation, and readers SHOULD cross-check
against it rather than rely on detection.

## 4. Plaintext Representation

The plaintext representation is a constrained subset of POSIX pax tar
[POSIX-PAX], extended with vendor keywords in the `REMANENCE.` namespace and a
generated CBOR manifest stored as the archive's final member. The single
structural constraint beyond plain tar is that every file's payload begins on
a body-block boundary (Section 4.6.3); the unconstrained tar stream remains
extractable by any pax-aware tool, which simply ignores the vendor keywords
(Section 4.10).

### 4.1. Frame Sequence

A plaintext RAO object is a byte string with the following layout, where every
frame boundary falls on a 512-byte record boundary:

```text
+--------------------------------------------------+
| Global pax header (typeflag 'g')                 |  Section 4.5
+--------------------------------------------------+
| Entry 0:  pax header ('x') + ustar ('0'/'1'/'2'/'5') [+ data for '0']  |  Section 4.6
+--------------------------------------------------+
| ...                                              |
+--------------------------------------------------+
| Entry N-1 (last member entry)                    |
+--------------------------------------------------+
| Manifest entry ('x' + '0' + CBOR data)           |  Section 4.7
+--------------------------------------------------+
| Tar EOF: two all-zero 512-byte records           |  Section 4.8
+--------------------------------------------------+
| Zero fill to the next chunk_size multiple        |  Section 4.8
+--------------------------------------------------+
```

The manifest entry MUST be the final entry before tar EOF. Member entries
appear in caller-supplied order. An object with zero member entries is valid:
it contains the global header, the manifest entry, and the EOF sequence.

### 4.2. Body Blocks and `chunk_size`

The object byte stream is written as consecutive fixed-size body blocks of
exactly `chunk_size` bytes. `chunk_size` MUST be a positive multiple of 512.
On tape `chunk_size` MUST equal the fixed tape block size of the containing
tape file (Section 8.2); one body block is one tape block. The plaintext
representation defines no maximum; the envelope header encodes `chunk_size`
as a 32-bit field, so an object stored in the encrypted representation is
bounded to `chunk_size` ≤ 2^32 − 512 (Section 5.2). Operational bounds come
from drive block-size limits.

The total object length is always an exact multiple of `chunk_size`
(Section 4.8), and the object's block count is knowable before any payload
byte is written (Section 4.8). A Reader is given the object's `chunk_size` and
block count out of band (catalog, bootstrap, or filemark map) and MUST process
exactly that many blocks; the format reserves no meaning for bytes outside the
object's blocks and provides no in-band mechanism for locating object
boundaries (Section 4.1, Section 8.2).

### 4.3. The ustar Record Subset

RAO emits POSIX ustar headers [POSIX-PAX] restricted as specified here.
Readers MUST validate the checksum of every non-zero header record
(Section 4.3.3); the reader-ignored fields are governed by the rules of
Section 4.3.2.

#### 4.3.1. Header Layout

Every header is one 512-byte record:

| Offset | Length | Field | Writer-normative value |
| ---: | ---: | --- | --- |
| 0 | 100 | `name` | Entry-dependent (Section 4.3.2); NUL-padded |
| 100 | 8 | `mode` | Regular entries: `0000644\0`, or `0000755\0` when `REMANENCE.executable` is `true`; hardlinks: `0000644\0`; symlinks: `0000777\0`; directories: `0000755\0`; pax header records (`g`, `x`): `0000644\0` |
| 108 | 8 | `uid` | `0000000\0` |
| 116 | 8 | `gid` | `0000000\0` |
| 124 | 12 | `size` | 11 octal digits + NUL (Section 4.3.2) |
| 136 | 12 | `mtime` | `00000000000\0` (mtime lives in pax only, Section 4.4.4) |
| 148 | 8 | `chksum` | Section 4.3.3 |
| 156 | 1 | `typeflag` | `g`, `x`, `0`, `1`, `2`, or `5` (Section 4.5, Section 4.6) |
| 157 | 100 | `linkname` | Symlink or hardlink target when it fits in ustar `linkname`; otherwise `PAX_LINK_PLACEHOLDER` (Section 4.6.1); all NUL for other entries |
| 257 | 6 | `magic` | `ustar\0` |
| 263 | 2 | `version` | `00` |
| 265 | 32 | `uname` | `remanence`, NUL-padded |
| 297 | 32 | `gname` | `remanence`, NUL-padded |
| 329 | 8 | `devmajor` | All NUL |
| 337 | 8 | `devminor` | All NUL |
| 345 | 155 | `prefix` | All NUL (writers never use `prefix`) |
| 500 | 12 | — | All NUL |

Octal fields are zero-padded ASCII octal terminated by NUL. When parsing,
Readers MUST stop a numeric field at the first NUL or space, MUST accept
surrounding ASCII whitespace, and MUST treat an empty field as zero. The
`uid`, `gid`, ustar `mtime`, `uname`, and `gname` fields carry the fixed
values above regardless of metadata-preservation tier: RAO 1.0 deliberately
does not preserve ownership, so a root-run standard `tar` extraction cannot
apply ownership the format never recorded. Readers MUST ignore these fields.

#### 4.3.2. Reader-Ignored Fields; Names and Sizes

Readers MUST NOT base acceptance on `mode`, `uid`, `gid`, ustar `mtime`,
`uname`, `gname`, `devmajor`, `devminor`, or `version`. Readers MUST honor
`prefix` when forming a header path from a foreign ustar header
(`prefix + "/" + name` when `prefix` is non-empty), even though conformant
writers leave it empty.

The authoritative path and size of an entry are its pax `path` and `size`
records (Section 4.4.4). The ustar header nevertheless remains well-formed:

- **Name.** If the effective path is non-empty, at most 100 bytes, and
  consists solely of non-control ASCII, the writer MUST store it in `name`
  verbatim; otherwise the writer MUST store `PAX_PATH_PLACEHOLDER`
  (`remanence/pax-path`).
- **Size.** If the payload length is ≤ `USTAR_SIZE_MAX`, the writer MUST store
  it in `size`; otherwise the writer MUST store zero. The pax `size` record is
  always present and authoritative, so files ≥ 8 GiB are fully supported.
- The ustar names of the pax header records themselves are the fixed
  constants `GLOBAL_HEADER_NAME`, `PAX_HEADER_NAME`, and
  `MANIFEST_PAX_HEADER_NAME` (Section 2.5). They exist for human-readable
  `tar -t` listings; Readers MUST NOT interpret them.

#### 4.3.3. Checksum

The `chksum` field holds the unsigned sum of all 512 header bytes with the
eight checksum bytes treated as ASCII spaces (0x20), encoded as six ASCII
octal digits, a NUL, and a space. Writers MUST emit exactly this encoding.
Readers MUST verify the unsigned checksum and reject a mismatch with
`UstarChecksumMismatch`.

#### 4.3.4. Typeflags

| Typeflag | Meaning |
| --- | --- |
| `g` (0x67) | Global pax header (Section 4.5) |
| `x` (0x78) | Per-entry pax extended header (Section 4.4) |
| `0` (0x30) | Regular file |
| `1` (0x31) | Hardlink (Section 4.6) |
| `2` (0x32) | Symbolic link |
| `5` (0x35) | Directory |
| NUL (0x00) | Accepted by Readers as a regular file (pre-POSIX compatibility); writers MUST NOT emit it |

Readers MUST reject any other typeflag with `UnsupportedTarTypeflag`. This is
deliberate: version 1.0's entry set is regular files, hardlinks, symbolic
links, and directories — a faithful tree of files — and excludes device,
FIFO, socket, and other special entries (Section 1.5); accepting an
unsupported typeflag silently would misrepresent an unsupported archive as
fully restored.

### 4.4. Pax Extended Records

#### 4.4.1. Record Grammar

A pax header's payload is a sequence of records, each:

```text
"<len> <keyword>=<value>\n"
```

where `<len>` is the decimal byte length of the entire record including the
length digits themselves, the single space, and the trailing newline. `<len>`
is self-referential; writers MUST compute it by fixed-point iteration over its
own digit count, starting from the digit count of the record length with zero
length digits (`len ← base + digits(len)`, with `digits` initialized to
`digits(base)`, iterated until stable). At certain base lengths (8, 97, 996,
9995, …) two self-consistent values of `<len>` exist; the upward iteration
converges to the smaller, and writers MUST emit that smaller value.

Constraints, enforced by both Writers and Readers:

- `<keyword>` MUST be non-empty ASCII and MUST NOT contain `=`, newline, or
  NUL.
- `<value>` MUST be valid UTF-8 and MUST NOT contain any byte < 0x20 (a pax
  value in this format is always single-line).
- `<len>` MUST be ≥ 1 and MUST NOT exceed the remaining header payload.
- The record MUST end with exactly one newline at offset `<len> − 1`.

Readers MUST reject violations with `PaxRecordMalformed`.

#### 4.4.2. Emission Order and Duplicates

Writers MUST emit the records of one pax header sorted in ascending bytewise
order of the keyword, and MUST NOT emit the same keyword twice in one header.
(A consequence: all `REMANENCE.*` keywords sort before the lowercase standard
keywords `mtime`, `path`, `size`.) Readers MUST apply POSIX last-wins
semantics if duplicates are encountered in a foreign archive, and MUST NOT
reject an archive solely for unsorted records. Determinism is a writer
obligation, not a read-acceptance rule.

#### 4.4.3. Unknown Keywords

Readers MUST ignore unknown keywords, including unknown `REMANENCE.*` keywords
(this is how 1.x minor revisions extend the format, Section 10), and SHOULD
preserve them when re-emitting metadata. Unknown keywords MUST NOT alter
payload framing or interpretation.

#### 4.4.4. Standard Keywords Used

| Keyword | Presence | Meaning |
| --- | --- | --- |
| `path` | REQUIRED on every entry | Effective UTF-8 entry path; overrides the ustar `name` |
| `size` | REQUIRED on every entry | Effective payload byte length (decimal); overrides the ustar `size` |
| `linkpath` | Symlink/hardlink entries when needed | Effective symlink target (an opaque string), or hardlink target (an in-object path, Section 4.6); overrides the ustar `linkname` |
| `mtime` | OPTIONAL | Modification time in POSIX pax decimal form: non-negative decimal seconds since the epoch, optionally followed by `.` and fractional digits. The value is a caller-supplied string; Writers MUST validate this shape and MUST emit the validated string verbatim, so the byte stream is a deterministic function of the caller's input |

Writers MUST always emit `path` and `size` even when the ustar header could
carry them, so every entry is self-describing under pax rules alone. Readers
MUST use the pax values when present and fall back to the ustar fields
otherwise (foreign-archive tolerance). For symbolic links and hardlinks,
Readers MUST use `linkpath` when present and fall back to the ustar `linkname`
field.

The object-level metadata-preservation tier
(`REMANENCE.metadata_preservation`, Section 4.5.1) declares intent: `minimal`
(path + content), `archival` (adds `mtime` and `REMANENCE.executable`), `full`
(reserved — adds no additional 1.0 wire fields). Presence of
`mtime`/`REMANENCE.executable` on an entry is governed by the caller, not
validated against the declared tier.

### 4.5. The Global Header

#### 4.5.1. Keywords

The first record group of every object is a global pax header (typeflag `g`)
whose payload carries exactly these eight keywords, emitted in bytewise
keyword order (Section 4.4.2):

| Keyword | Constraint |
| --- | --- |
| `REMANENCE.caller_object_id` | Non-empty opaque UTF-8; identifier assigned by the archiving system above this format |
| `REMANENCE.chunk_size` | Decimal `chunk_size` in bytes; on tape this MUST equal the containing tape file's block size (Sections 4.2, 8.2) |
| `REMANENCE.encryption` | MUST be `none` (Section 4.5.2, Section 10) |
| `REMANENCE.format_id` | MUST be `rao-v1` |
| `REMANENCE.metadata_preservation` | One of `minimal`, `archival`, `full` |
| `REMANENCE.object_id` | Non-empty UTF-8 object identifier (a UUID string in practice; opaque to this format) |
| `REMANENCE.schema_version` | `<major>.<minor>` decimal text; MUST have major version 1 (Section 10) |
| `REMANENCE.write_timestamp` | [RFC3339] timestamp of object creation |

#### 4.5.2. Validation

Before delivering any entry, a Reader MUST verify on the accumulated global
records:

1. `REMANENCE.format_id` is present and equals `rao-v1` (`UnsupportedFeature`
   otherwise; a missing key is `Parse`).
2. `REMANENCE.schema_version` is present and its major component (the decimal
   text before the first `.`, or the whole value if no `.`) parses as an
   unsigned integer equal to 1 (`UnsupportedFeature` on mismatch, `Parse` on
   malformed).
3. If `REMANENCE.encryption` is present, it equals `none`
   (`UnsupportedFeature` otherwise). This is a refusal gate: a Reader that
   ignored it could restore ciphertext as content under a future revision.
   Confidentiality is provided exclusively by the Section 5 envelope *around*
   the stream, never flagged inside it.
4. If `REMANENCE.chunk_size` is present, it equals the externally supplied
   `chunk_size` (`ChunkSizeMismatch` otherwise). A mismatch means the object
   is mis-cataloged or the tape was rewritten with different geometry;
   restoring under the wrong geometry mis-addresses every chunk.

The remaining global keywords (`object_id`, `caller_object_id`,
`metadata_preservation`, `write_timestamp`) are descriptive; Readers MUST NOT
require them for acceptance. Consumers cross-check the identity keywords
against the manifest (Section 4.7). A conformant writer emits exactly one
global header, first. Readers MUST accept a foreign archive containing
additional `g` headers later in the stream by merging their records with
last-wins semantics and re-running these checks before the next entry is
delivered.

### 4.6. File Entries and Block Alignment

#### 4.6.1. Entry Frame

Each entry is, in order:

1. A pax extended header: ustar record (typeflag `x`) whose `size` is the pax
   payload length; the pax payload; zero padding to the next 512-byte
   boundary.
2. An entry ustar header (typeflag `0`, `1`, `2`, or `5`, Section 4.3).
3. For regular files only, the payload: exactly `size` bytes (the effective
   size). Hardlink, symlink, and directory entries MUST have `size = 0` and no
   payload.
4. Zero padding to the next 512-byte boundary (none if `size mod 512 = 0`).

For symbolic links, Writers MUST store the target in ustar `linkname` when it
fits in 100 bytes; otherwise they MUST store it in pax `linkpath` and store
`PAX_LINK_PLACEHOLDER` (`remanence/pax-linkpath`) in `linkname`. A symlink target is an opaque UTF-8 OS string, not
an RAO path: it MAY be absolute, contain `..`, or be dangling. For directories,
Writers SHOULD emit entries only for directories that cannot be inferred from
child paths, i.e. empty directories; directory paths MUST end in `/`.

**Hardlinks.** A hardlink entry records that its path is a second name for the
bytes of another entry — the **primary** — in the same object. Its target is
stored exactly as a symlink target is (`linkname`, or pax `linkpath` with
`PAX_LINK_PLACEHOLDER` in `linkname`), but unlike a symlink target it is **not** an
arbitrary string: it MUST be a canonical relative path (Section 4.6.6) that
resolves, within the same object, to a **regular-file primary entry appearing
before** the hardlink entry. Of a set of names sharing one underlying file the
**primary** is one regular entry that holds the bytes; each other name is a
hardlink entry. Primary selection MUST be deterministic: the first such name in
archive order, or — if that one is omitted — the first surviving name; if only
one name survives it is a plain regular entry (no hardlink entry).

#### 4.6.2. Per-Entry Keywords

In addition to `path`, `size`, and optional `mtime` (Section 4.4.4), every
entry's pax header carries:

| Keyword | Presence | Constraint |
| --- | --- | --- |
| `REMANENCE.chunk_count` | REQUIRED | Decimal; MUST equal the Section 4.6.4 value |
| `REMANENCE.compression` | REQUIRED | MUST be `none` (Section 10) |
| `REMANENCE.executable` | OPTIONAL | `true` or `false` |
| `REMANENCE.file_id` | REQUIRED | Non-empty opaque UTF-8 stable identifier, unique within the object |
| `REMANENCE.file_sha256` | Regular entries only | Exactly 64 lowercase hex digits: SHA-256 of the exact payload bytes |
| `REMANENCE.is_manifest` | Manifest entry only | MUST be `true`; MUST be absent on member entries |
| `REMANENCE.pad` | Non-empty regular entries, as needed | Alignment filler (Section 4.6.3); value MUST consist solely of ASCII spaces. Zero-payload entries carry no pad record |

Readers MUST verify `REMANENCE.compression` is present and equals `none` on
every entry before delivering its payload (`UnsupportedFeature` otherwise;
missing key is `Parse`). Readers MUST ignore the *content* of `REMANENCE.pad`.
Readers SHOULD cross-check `REMANENCE.chunk_count` against the value
recomputed from the effective size (Section 4.6.4) and surface a mismatch as
an inconsistency. `REMANENCE.file_sha256` is not consulted during framing; its
verification for regular entries is a delivery-time obligation (Section 4.9)
and the core of the Verifier profile (Section 7.4). Non-regular entries omit
`REMANENCE.file_sha256`; their metadata integrity is covered by the manifest,
the complete `plaintext_digest`, and, for plaintext copies, the stored-object
integrity chain.

#### 4.6.3. The Alignment Rule

**Invariant (normative):** for every entry whose effective size is greater
than zero, the payload start offset `D` (the first byte after the entry ustar
header) satisfies

```text
D ≡ 0   (mod chunk_size)
```

Zero-payload entries — empty regular files, hardlinks, symbolic links, and
directories — are exempt and use plain 512-byte tar-record alignment; their
pax headers carry no `REMANENCE.pad` record. Readers MUST reject
an entry whose effective size is greater than zero and whose payload offset is
not chunk-aligned with `ChunkAlignmentViolation`.

For non-empty entries, alignment is achieved entirely inside the entry's own pax header by sizing the
`REMANENCE.pad` record. Let `O` be the byte offset of the entry's pax ustar
record (always a multiple of 512), `B` the pax payload length after including
the pad record, and `R = roundup512(B)`. The writer MUST choose the pad length
such that

```text
O + 512 + R + 512 ≡ 0   (mod chunk_size)
```

(the two 512s are the pax ustar record and the file ustar record). Because the
byte stream must be deterministic, the pad length is uniquely determined: let
`Rmin` be `roundup512` of the pax payload including an empty-valued pad record;
the target `R` is the smallest multiple of 512 that is ≥ `Rmin` and satisfies
the congruence; the pad value is the largest number of spaces for which the
pax payload length does not exceed `R` (solving each candidate record's
self-referential length per Section 4.4.1). If that payload does not round up
to exactly `R` (a decimal-digit-boundary corner), the writer advances `R` by
`chunk_size` and retries; a writer MUST fail with `Layout` rather than emit a
misaligned entry if no solution exists within `4 × chunk_size` above `Rmin`.
The pad is never a standalone tar member — it is a legitimate pax record of
the entry it aligns, invisible to standard tools.

There is no padding *after* payloads beyond tar's normal 512-byte record
padding: a body block may contain one file's tail bytes followed immediately
by the next entry's headers. Readers recover exact sizes from `size`, never
from block boundaries.

#### 4.6.4. Chunk Geometry

For an entry with effective size `Z` and payload offset `D`:

```text
chunk_count     = 0                       if Z = 0
                  ceil(Z / chunk_size)    otherwise
first_chunk_lba = absent                  if Z = 0
                  D / chunk_size          otherwise
```

`first_chunk_lba` is an inner `BodyLba`. Byte range `[s, s+n)` of the file
maps to body blocks
`first_chunk_lba + floor(s / chunk_size) ..= first_chunk_lba + floor((s+n−1) / chunk_size)`
with head/tail trimming; the final chunk holds
`Z − (chunk_count−1) × chunk_size` payload bytes (plus whatever follows in the
stream). Range requests MUST be validated against `Z` with checked arithmetic
before mapping (Section 6.1).

#### 4.6.5. Payload Hashing

For regular entries, `REMANENCE.file_sha256` is computed over the exact `Z`
payload bytes — never over tar headers, padding, or block fill. When the
writer receives payload bytes as a stream, it MUST recompute the SHA-256 of
the bytes actually consumed and MUST fail the object (refusing to complete it)
if the recomputed digest or byte count differs from the declared spec. This
proves the writer archived the payload it was given, not the payload the
metadata describes. Symlink, directory, and hardlink entries carry no payload
and no payload hash of their own; a hardlinked name's content, hash, and PFR
coordinates are its primary's, reached through `link_target` (Section 4.7.2).

#### 4.6.6. Path and Identity Rules

For every member entry, writers MUST enforce:

1. `path` is non-empty UTF-8, contains no NUL and no byte < 0x20.
2. `path` is a **canonical relative path**: it does not begin with `/`, does
   not end with `/` unless the entry is a directory, and, after ignoring a
   directory's one required trailing slash, none of its `/`-separated components
   is empty, `.`, or `..`.
3. `path` is not `_remanence` and does not start with `_remanence/` (the
   reserved namespace; the manifest is the only `_remanence/` entry in 1.0).
4. No two entries in one object share a `path`.
5. No two entries in one object share a `REMANENCE.file_id`; the manifest's
   `file_id` MUST also be distinct from every payload `file_id`.

Readers MUST reject an entry whose effective path violates rules 1–2
(`InvalidPath`): a traversal-shaped or non-canonical path is nonconformant,
and accepting it would push the hazard onto every downstream consumer
(Section 12.10). Rules 3–5 are writer-side; Verifiers and Consumers catch
duplicates via the manifest (Section 4.7). Within those rules, paths are byte
sequences stored verbatim: the format performs no Unicode normalization (NFC
and NFD spellings of the same name are distinct paths), no case folding, and
no separator translation.

Symlink targets are not entry paths and MUST NOT be validated with the
canonical-relative-path rule. A target is a pax value: valid UTF-8, no NUL,
and no byte < 0x20. It may be absolute, contain `..`, or point to a missing
target; restore safety is a Consumer obligation (Section 12.10).

#### 4.6.7. Entry Order

Payload entries appear in caller-supplied order; the format assigns no meaning
to the order beyond determinism. The manifest MUST be the last entry. Readers
identify the manifest by its exact path `_remanence/manifest.cbor` and MUST
NOT rely on `REMANENCE.is_manifest` alone. Readers MUST reject any entry
appearing after the manifest entry (`Parse`): such an entry cannot be listed
in the manifest, so the object's self-description would be silently
incomplete.

### 4.7. The Manifest

The manifest is a generated regular-file entry, last in the archive, with:

- `path` = `_remanence/manifest.cbor`
- `REMANENCE.is_manifest` = `true`
- `REMANENCE.executable` = `false`
- `REMANENCE.file_sha256` = SHA-256 of the manifest CBOR bytes
  (`manifest_sha256`)
- the standard alignment, hashing, and chunk-geometry rules of Section 4.6.

The manifest **excludes itself**: its `file_entries` array lists every member
entry — regular files, hardlinks, symlinks, and directories — except the
manifest entry itself. Its own identity lives in its pax header and, externally, in the
parity-layer bootstrap row (plaintext copies, Section 8.2), which enables
direct LOCATE-to-manifest reading without scanning the archive.

#### 4.7.1. Deterministic CBOR

A manifest is a single CBOR [RFC8949] data item in the **manifest profile** of
RAO's deterministic CBOR. **Item repertoire** — each item MUST be one of:

| Major type | Permitted |
| --- | --- |
| 0 | Unsigned integers 0 through 2^64 − 1 |
| 2 | Definite-length byte strings |
| 3 | Definite-length UTF-8 text strings |
| 4 | Definite-length arrays |
| 5 | Definite-length maps with **text-string keys** |
| 7 | Simple values `false` (20), `true` (21), `null` (22) only |

Negative integers (major type 1), tags (major type 6), floats,
indefinite-length items, `undefined`, and all other simple values MUST NOT
appear; decoders MUST reject them with `Cbor`.

**Encoding requirements** (decoders MUST reject violations with `Cbor`):

1. Every integer value and every length argument uses the shortest possible
   encoding (RFC 8949 preferred serialization).
2. Map keys are sorted in strictly ascending bytewise lexicographic order of
   their **deterministic encodings** (RFC 8949 §4.2.1) — for text keys this
   orders by encoded length prefix first, then key bytes, *not* plain
   alphabetical order. Duplicate keys MUST NOT appear.
3. Text strings are valid UTF-8.
4. The item occupies the entire manifest payload exactly; no trailing bytes.

Canonical form MUST be validated over the original encoded bytes, not by
decode-and-re-encode. **Structural limits**: an object MUST NOT contain more
than `MAX_FILE_ENTRIES` (10,000,000) member entries; manifest nesting depth
MUST NOT exceed `MANIFEST_MAX_DEPTH` (8), counting the top-level map as
depth 1. Decoders MUST enforce both incrementally and MUST bound allocations
by the manifest's declared size, never by counts read from the CBOR stream.

#### 4.7.2. Schema

The top-level item is a map with exactly these seven text keys (shown in
encoded sort order):

| Key | Type | Constraint |
| --- | --- | --- |
| `object_id` | text | MUST equal the global `REMANENCE.object_id` |
| `chunk_size` | unsigned | MUST equal the global `REMANENCE.chunk_size` |
| `file_entries` | array | One file-entry map per member entry (regular/hardlink/symlink/directory), in archive order |
| `schema_version` | unsigned | MUST be 1 (`MANIFEST_SCHEMA_VERSION`) |
| `object_metadata` | map | Reserved; MUST be empty (`{}`) in 1.0 writers |
| `caller_object_id` | text | MUST equal the global `REMANENCE.caller_object_id` |
| `external_references` | array | Reserved; MUST be empty (`[]`) in 1.0 writers |

Each `file_entries` element is a map with the base keys below, plus the
conditional non-regular keys. (Keys are shown grouped by function; on the
wire they appear in the deterministic order of Section 4.7.1.) Regular entries MUST NOT carry `entry_type` or
`link_target`, preserving the pre-expansion byte representation for
regular-only objects.

| Key | Type | Constraint |
| --- | --- | --- |
| `path` | text | Effective entry path |
| `file_id` | text | Entry `REMANENCE.file_id` |
| `executable` | `true`/`false`/`null` | `null` when the writer was given no value |
| `size_bytes` | unsigned | Effective payload length (0 for hardlink/symlink/directory/empty entries) |
| `chunk_count` | unsigned | Section 4.6.4 value (0 for zero-payload entries, hardlinks included) |
| `entry_type` | text | OPTIONAL; absent means `regular`; otherwise `hardlink`, `symlink`, or `directory` |
| `file_sha256` | bytes | Regular entries only: exactly 32 bytes; binary SHA-256 (hex in pax, binary here). A hardlinked name's hash is its primary's, reached via `link_target` |
| `first_chunk_lba` | unsigned/`null` | Inner `BodyLba`; `null` if and only if `size_bytes` = 0 (so `null` for hardlinks) |
| `link_target` | text | Symlink entries: the effective target string. Hardlink entries: the primary's in-object path (Section 4.6) |
| `metadata_preservation_data` | map | Reserved; MUST be empty (`{}`) in 1.0 writers |

Consumer obligations:

1. Before interpreting any field, a Consumer MUST verify the manifest bytes
   against an anchor digest: the bootstrap/catalog `manifest_sha256` when
   available, or — self-consistency only — the manifest entry's own pax
   `REMANENCE.file_sha256` (`ManifestDigestMismatch` on failure). An
   unverified manifest is untrusted input from removable media.
2. A Consumer MUST reject a manifest violating the type or value constraints
   above (`ManifestInvalid`), including the cross-checks: `object_id`,
   `caller_object_id`, and `chunk_size` MUST equal the corresponding global
   header values when both are in hand, and no two `file_entries` elements
   may share a `path` or a `file_id`.
3. A Consumer MUST treat unknown additional keys (top-level or per-entry) as a
   1.x extension and ignore them (Section 10), and MUST NOT reject a manifest
   whose reserved maps/arrays are non-empty — that is the designated 1.x
   extension surface.
4. When both the manifest and the archive entries are available, a Consumer
   SHOULD verify they correspond exactly — same paths, entry types, link
   targets, sizes, hashes where present, and chunk geometry, with no extras on either side (Verifiers MUST;
   Section 7.4).

#### 4.7.3. Within-Stream Chain of Trust

```text
bootstrap/catalog anchor (manifest location + manifest_sha256; plaintext copies — Section 8.2)
        │  externally anchored, parity-protected on tape
        ▼
manifest.cbor  ── byte-verified by manifest_sha256
        │
        ▼
per-entry  type, path, target; regular entries add file_sha256, size_bytes,
           first_chunk_lba, chunk_count (a hardlink resolves via target to its primary)
        │
        ▼
regular payload bytes ── byte-verified by file_sha256
```

For regular entries, pax `REMANENCE.file_sha256` keywords duplicate the
manifest hashes as a within-stream cross-check, allowing per-file verification
even when the manifest's blocks are damaged (and vice versa). Non-regular
entry metadata is covered by the manifest and the whole-object digest. This
chain provides integrity, not authentication (Section 12.6); for encrypted
copies the anchor is the envelope's authenticated `plaintext_digest`
(Section 7.1).

### 4.8. End of Archive

After the manifest entry's padding, writers MUST emit exactly two all-zero
512-byte records. Readers MUST treat an all-zero header record followed by a
second all-zero record as end of archive, and MUST reject an all-zero record
followed by a non-zero record with `Parse` (a single zero tar EOF record).

After the EOF records, writers MUST fill the remainder of the final body block
with zero bytes, so the object's total length is

```text
total_size_bytes      = roundup(offset_after_EOF, chunk_size)
projected_size_blocks = total_size_bytes / chunk_size
```

This is the only block-level zero fill in the stream, and it is tar-safe: it
lies beyond the archive EOF where standard tar already stops. Readers MUST NOT
interpret bytes after the EOF records; Verifiers (Section 7.4) MUST confirm
the fill is all-zero and report a nonzero fill as a nonconformity. A writer
whose emitted block count differs from its planned `projected_size_blocks`
MUST fail the object rather than complete it.

### 4.9. Writer, Planner, and Reader Obligations

**Writer / Planner.** The Planner computes the entire layout — every offset,
pad size, manifest byte, and the final block count — from the file *specs*
alone (path, file_id, entry type, link target where present, size, hash,
optional mtime/executable, and 1.x preservation metadata), without payload
bytes; Planner and Writer MUST share the same sizing rules such that the
planned layout is byte-exact. The writer's workflow: validate options
(`chunk_size`; non-empty `object_id`, `caller_object_id`, `write_timestamp`,
`manifest_file_id`); validate every member spec (Section 4.6.6); plan the
layout (which serializes the manifest and computes `manifest_sha256`); emit
the global header, each member entry, and the manifest entry, streaming
payload bytes through the running SHA-256 check of Section 4.6.5; emit tar EOF
and the final zero fill; verify the emitted block count equals the plan;
report the layout (`projected_size_blocks`, per-file `first_chunk_lba`,
manifest geometry, `manifest_sha256`) to the caller for cataloging. A failed
object MUST NOT be reported as complete. The writer consumes a block sink that
reports per-block outcomes; a block write that commits fewer bytes than the
full block, or reports hard end-of-medium, MUST fail the object
(`IncompleteBlockWrite`).

**Reader.** A Reader receives a block source positioned at the object's inner
`BodyLba(0)`, the object's `chunk_size`, and its block count. Two I/O
profiles exist, with identical acceptance rules. The **streaming** profile is
RECOMMENDED; it requires memory proportional to `chunk_size` plus one pax
header. The **materializing** profile exists for compatibility; a
materializing Reader MUST bound its up-front allocation with a fallible
reservation. A Reader operates in
one of two modes: **restore** (the default; integrity-verifying) or
**salvage** (a deliberately-selected, explicitly-labeled mode for damaged
media in which verification failures are reported but delivery continues; an
implementation MUST NOT make salvage the default or silently fall back to it).
Procedure:

1. Read 512-byte records. A short block read is a hard error.
2. On an all-zero record: require the second EOF record (Section 4.8), run the
   Section 4.5.2 global checks (covers empty objects), and stop. Remaining
   blocks are ignored.
3. Verify the header checksum (Section 4.3.3).
4. Dispatch on typeflag: `g` → merge records into the global set (last-wins),
   defer re-validation to the next entry; `x` → parse records, attach to the
   next entry; `0`/NUL → a regular entry: run the global checks if not yet run
   for the current global set, compute effective path and size, verify
   `REMANENCE.compression`, verify chunk alignment if `size > 0`, deliver
   exactly `size` payload bytes, then skip the record padding (EOF inside a
   declared payload or its padding is `TruncatedPayload`); `1` → a hardlink:
   require `size = 0`, compute the effective path and in-object target
   (`linkpath` or `linkname`), verify the target resolves to a regular-file
   primary already delivered (`InvalidHardlinkTarget` otherwise), and deliver a
   hardlink entry with no payload (its content/PFR resolve through `link_target`
   to that primary); `2` →
   a symlink: require `size = 0`, compute effective path and target (`linkpath`
   or `linkname`), and deliver a symlink entry with no payload; `5` → a
   directory: require `size = 0` and deliver a directory entry with no
   payload; anything else → `UnsupportedTarTypeflag`.
5. **Integrity (restore mode).** For every regular entry delivered in full,
   compute SHA-256 over the delivered payload bytes while streaming and
   compare against `REMANENCE.file_sha256`; on mismatch, fail the entry with
   `FileDigestMismatch` before reporting it restored (in salvage mode:
   deliver, but report the mismatch). Hardlink, symlink, and directory entries
   have no payload hash of their own; they are verified through the
   manifest/object digest chain (and, for a hardlink, its referential
   integrity — Section 4.6).
   Partial-range reads cannot verify a whole-file hash; their integrity comes
   from the parity layer's block CRCs, and range-read implementations MUST say
   so rather than imply hash-verified content.
6. Capture the entry whose effective path is `_remanence/manifest.cbor` as the
   manifest bytes. An object whose EOF is reached with no manifest entry is
   nonconformant: Verifiers MUST reject it (Section 7.4), and a restore-mode
   Reader SHOULD report the absence to its caller. The reference Reader reports
   this non-fatally as the typed `MissingManifest` warning; the absence remains
   visible even when member payloads can otherwise be restored.

A conformant Reader accepts mildly foreign archives where safe
(unsorted/duplicate pax records, NUL typeflag, `prefix`-formed names, missing
pax `path`/`size` with ustar fallback, later `g` headers) and rejects the
cases in which silent acceptance would misrepresent the object (unknown
typeflags, unknown format/major, non-`none`
compression, misaligned data, traversal-shaped paths, checksum mismatch).

### 4.10. Standard-Tool Extraction (Long-Term Fallback)

The plaintext byte stream is a valid pax archive. With GNU tar, bsdtar, or any
pax-aware reader:

```sh
mt -f /dev/nst0 fsf <n>                 # position to the object's tape file
tar -b <chunk_size/512> -xf /dev/nst0   # e.g. -b 512 for 256 KiB blocks
```

extracts every payload file byte-correct, recreates hardlinks, symlinks, and
directories, and also writes one extra file `_remanence/manifest.cbor`. Unknown
`REMANENCE.*` keywords are ignored by POSIX rule; `REMANENCE.pad` inflates
only header size, never content; the manifest decodes with any generic CBOR
tool into self-describingly-named text fields. Stock tar faithfully restores
absolute or dangling symlinks too; that fidelity is correct but not a safety
claim (Section 12.10). With all Remanence metadata lost, a Scanner can still
walk the archive using only tar rules — header, `size`, `roundup512(size)`,
repeat — recovering payload bytes, hardlink relationships, symlink targets,
directory entries, and names; it loses only chunk addressing (irrelevant when scanning) and
verification (recoverable from the manifest if its blocks survive).
Conformance requires demonstrated extraction equality by GNU tar, bsdtar, and
Python `tarfile` (Section 14).

## 5. Encrypted Representation

### 5.1. Frame Sequence

An encrypted RAO object is a byte string with the following layout:

```text
+--------------------------------------------------+
| Plaintext header, 128 bytes                      |  Section 5.2
+--------------------------------------------------+
| Metadata frame (AEAD ciphertext), M bytes        |  Section 5.5
+--------------------------------------------------+
| Payload frame (chunked AEAD ciphertext)          |  Section 5.6
+--------------------------------------------------+
| Completion footer, 16 bytes                      |  Section 5.7
+--------------------------------------------------+
| Zero fill to the next chunk_size multiple        |  Section 5.7
+--------------------------------------------------+
```

The header, footer, and fill are plaintext. Everything the object *says* — the
metadata frame and the payload frame (which contains the entire canonical
plaintext object, manifest included) — is encrypted and authenticated. An
encrypted RAO object reveals no filenames, file count, or structure (see
Section 12.5 for exactly what it does reveal). The total stored length is an
exact positive multiple of `chunk_size`, as in the plaintext representation,
so that both representations map uniformly onto fixed-size storage blocks and
parity (Sections 8, 9).

### 5.2. The Plaintext Header

The header is exactly 128 bytes. Readers MUST read exactly 128 bytes and MUST
reject any input whose `header_len` field is not 128.

| Offset | Length | Name | Type | Description |
| --- | ---: | --- | --- | --- |
| `0x00` | 4 | `magic` | ASCII | MUST be `RAO1`. |
| `0x04` | 2 | `header_len` | `uint16` | MUST be 128. |
| `0x06` | 1 | `format_version` | `uint8` | MUST be 1. |
| `0x07` | 1 | `suite_id` | `uint8` | MUST be `0x01` (HKDF-SHA-256 + ChaCha20-Poly1305). |
| `0x08` | 4 | `chunk_size` | `uint32` | The object's body-block size `C`. MUST be a positive multiple of 512 and MUST equal the inner stream's `REMANENCE.chunk_size` (Section 5.9). |
| `0x0C` | 4 | `flags` | `uint32` | MUST be zero. |
| `0x10` | 16 | `key_id` | bytes | Opaque archive key identifier. MUST NOT be all zero. |
| `0x20` | 16 | `hkdf_salt` | bytes | Per-object salt, derived per Section 5.4.1. MUST NOT be all zero. |
| `0x30` | 8 | `metadata_frame_len` | `uint64` | Stored metadata frame length `M`. Bounds: Section 5.5.3. |
| `0x38` | 8 | `reserved` | bytes | MUST be all zero. |
| `0x40` | 64 | `object_id` | UTF-8 | The object identifier, 1–64 bytes, NUL-padded to 64. MUST equal the inner `REMANENCE.object_id` (Section 5.9). |

**Frozen fields.** `header_len`, `format_version`, and `suite_id` have exactly
one valid value each for the lifetime of the `RAO1` magic. They exist for
self-description and damage detection, not agility: a corrupted byte in any of
them is a hard parse error, never a silent reinterpretation. Future revisions
MUST NOT assign additional valid values; any such change requires a new magic
(`RAO2`) — Section 10.

**`object_id` field rules.** The value is the inner stream's
`REMANENCE.object_id`, encoded as 1–64 bytes of UTF-8 containing no NUL,
right-padded with NUL bytes to 64. Readers MUST strip trailing NULs to recover
the value and MUST reject an all-NUL field, a field containing an interior NUL
(a NUL byte followed by any non-NUL byte), or invalid UTF-8, with
`InvalidObjectIdField`. Consequently an object whose `object_id` exceeds 64
UTF-8 bytes cannot be stored in the encrypted representation; writers MUST
reject such input at sealing time (`InvalidInput`). The plaintext stream
imposes no such bound; the cap is an envelope constraint. (Operationally,
object identifiers are UUID strings — 36 bytes.)

**Header validation order** (RECOMMENDED; error names per Section 11):

1. Read exactly 128 bytes (`UnexpectedEof`).
2. `magic` (`InvalidMagicBytes`).
3. `header_len` (`InvalidHeaderLength`).
4. `format_version` (`UnsupportedFormatVersion`).
5. `suite_id` (`InvalidSuite`).
6. `chunk_size` positive multiple of 512 (`InvalidChunkSize`).
7. `flags` zero, `reserved` zero (`ReservedBytesNotZero`).
8. `key_id` nonzero (`InvalidKeyIdentifier`); `hkdf_salt` nonzero
   (`InvalidSalt`).
9. `metadata_frame_len` within bounds (`MetadataFrameLengthInvalid`).
10. `object_id` field well-formed (`InvalidObjectIdField`).

When an input violates several requirements at once, a reader MAY report any
one error whose condition holds; conformance test vectors contain exactly one
fault each (Section 13.5).

### 5.3. Key Identification and the Key Registry

The `key_id` field is a 16-byte opaque archive key identifier; RAO assigns no
internal semantics to it. The key registry is an external system responsible
for generating `key_id` values, mapping each to root key material or a
retrieval procedure (a hardware security module, a key-management server,
escrow), and preserving key-epoch lifecycle
records. A Sealer MUST NOT store human-readable epoch labels in the header.

Implementations never fetch keys: callers supply root key material through an
in-memory interface (for example, a 32-byte key file provided by the operator
together with its `key_id`). Implementations MUST
reject root key material shorter than 32 bytes (`InvalidRootKey`), SHOULD use
exactly 32 uniformly random bytes, SHOULD zeroize key material when no longer
needed, and MUST NOT write key material to logs, diagnostics, or command
lines. Only `key_id` is persisted in the object.

### 5.4. Key Derivation

AEAD mode uses HKDF with SHA-256 [RFC5869]. `HKDF(ikm, salt, info, len)` means
HKDF-Extract with `salt` and `ikm`, then HKDF-Expand with `info` to `len`
bytes. Let:

```text
header_bytes = the exact 128 bytes stored at offsets 0x00 through 0x7F
header_hash  = SHA-256(header_bytes)
root_key     = key material resolved via key_id   (Section 5.3)
salt         = hkdf_salt from the header
```

```text
object_secret = HKDF(ikm = root_key,      salt = salt,  info = "rao1-object-v1" || header_hash, len = 32)
metadata_key  = HKDF(ikm = object_secret, salt = empty, info = "rao1-metadata-v1",              len = 32)
payload_key   = HKDF(ikm = object_secret, salt = empty, info = "rao1-payload-v1",               len = 32)
```

Three properties are deliberate and MUST be preserved by any future revision:

1. **The header is bound through derivation, not AAD.** `header_hash` is an
   input to `object_secret`, so changing any header bit — including a
   structurally valid change such as substituting another epoch's `key_id` or
   flipping a salt bit — changes every derived key and fails all subsequent
   authentication. The AEAD AAD can therefore stay empty (Sections 5.5.2,
   5.6.2); binding the header again as AAD would be redundant.
2. **Salt distinctness is derived, not assumed.** The salt is a PRF output
   binding the object's identifier, content, and metadata (Section 5.4.1), so
   distinct objects derive distinct salts — and therefore distinct keys —
   even if every environmental safeguard fails, up to the Section 12.1
   residual. The header carries no payload-*recoverable* data: the salt binds
   content through a PRF under `root_key` and reveals nothing without it
   (Section 12.5).
3. **Metadata and payload keys MUST remain separate**, derived with the
   distinct labels above, never unified or derived from one another. The
   metadata zero nonce is byte-identical to a non-final payload chunk 0 nonce;
   only the key separation makes that collision harmless (Section 12.2).

#### 5.4.1. Salt Derivation

The Sealer MUST derive `hkdf_salt` — it is never drawn from a random number
generator (Appendix B):

```text
metadata_hash = SHA-256(metadata plaintext bytes)        (Section 5.5.3)

hkdf_salt = HKDF(
    ikm  = root_key,
    salt = empty,
    info = "rao1-salt-v1" || ctr || object_id_field || plaintext_digest
           || metadata_hash,
    len  = 16
)
```

where `"rao1-salt-v1"` is `LABEL_SALT` (12 ASCII bytes, no terminator), `ctr`
is a single byte initially `0x00`, `object_id_field` is the exact 64-byte
NUL-padded header field of Section 5.2, `plaintext_digest` is the 32-byte
digest of the canonical plaintext object (known before sealing begins,
Section 5.8), and `metadata_hash` covers the serialized metadata plaintext
(serialized before salt derivation, Section 5.8 step 2; in version 1 the
metadata is itself a deterministic function of the canonical object,
Section 5.5.3 — the input exists so that *anything* the `metadata_key` will
ever encrypt is bound into the key derivation). In the improbable
case (probability 2^−128) that the result is all zero — reserved as
invalid in the header — the Sealer MUST increment `ctr` and re-derive. The
derivation is total and deterministic: given (`root_key`, `object_id`,
canonical bytes) — and the metadata, which version 1 fixes as a function of
those — the salt, and consequently the entire sealed object, is reproducible.

The following properties are normatively relied upon:

1. **Distinct objects derive distinct keys.** Objects with distinct
   `object_id`s derive distinct keys through header binding alone, whatever
   their salts. Objects improperly *sharing* an `object_id` (a defect in the
   identifier-assigning system, which is required to assign unique
   identifiers) still derive distinct
   salts whenever their `plaintext_digest` or metadata bytes differ —
   distinct content or metadata yields distinct digest inputs barring a
   SHA-256 collision — up to a 2^−128 pairwise collision of the 16-byte
   truncated PRF output, an accidental-only event, since the PRF is keyed by
   `root_key` and no party without the root key can compute, let alone search for,
   salt values (Section 12.1). No environmental condition (a cloned virtual machine, a fork
   without reseed, a broken entropy source) can cause two different objects to
   derive identical keys: sealing consumes no randomness at all.
2. **Resealing is byte-stable.** Sealing the same canonical object under the
   same epoch reproduces the byte-identical encrypted copy (identical salt,
   keys, ciphertext, `stored_digest`) — extending byte-stable fanout to
   independently produced encrypted copies, and making the Section 13 vectors
   reproducible by rule with no test-only interface. The disclosure this
   determinism implies — that two stored copies are reseals of one object —
   is already public via the header `object_id`.
3. **The salt discloses nothing.** It is a PRF output under `root_key`:
   without the root key it reveals nothing about the content, unlike a raw
   content digest in the header would (which would enable confirmation
   attacks against guessable payloads).

The salt MUST be derived exactly as specified; a Sealer MUST NOT accept a
caller-supplied salt; and the derivation is **verified, not trusted**: every
keyed open recomputes it and rejects a mismatch (Section 5.9 step 4), so a
defective sealer cannot produce a readable object that violates this section.
Section 12.1 analyzes the failure model.

### 5.5. The Metadata Frame

#### 5.5.1. Metadata CBOR Profile

The metadata plaintext is a single CBOR [RFC8949] data item in the **metadata
profile** of RAO's deterministic CBOR. The encoding requirements and
structural limits are those of Section 4.7.1, with two differences from the
manifest profile: top-level keys are unsigned integers rather than text
(Section 5.5.3), and the item repertoire additionally permits negative
integers (major type 1, values −1 through −2^64). Concretely, a metadata item
MUST be one of:

| Major type | Permitted |
| --- | --- |
| 0 | Unsigned integers 0 through 2^64 − 1 |
| 1 | Negative integers −1 through −2^64 |
| 2 | Definite-length byte strings |
| 3 | Definite-length UTF-8 text strings |
| 4 | Definite-length arrays |
| 5 | Definite-length maps |
| 7 | Simple values `false` (20), `true` (21), `null` (22) only |

Tags, indefinite-length items, floats, `undefined`, and all other simple
values MUST NOT appear; decoders MUST reject them with `InvalidCborEncoding`.
The encoding requirements (shortest-form integers and lengths; valid UTF-8;
map keys in strictly ascending bytewise order of their deterministic
encodings, no duplicates; the item occupying the plaintext exactly), the
canonical-form-over-original-bytes rule, and the structural limits
(`RAO_MAX_CBOR_NESTING_DEPTH` = 32; `RAO_MAX_METADATA_ITEMS` = 65536; bound
allocations by the declared frame length) all apply as in Section 4.7.1,
yielding `InvalidCborEncoding` on violation. One deterministic-CBOR validator
serves both profiles.

#### 5.5.2. Metadata Encryption

The stored metadata frame is a single ChaCha20-Poly1305 [RFC8439] AEAD
ciphertext over the metadata plaintext:

```text
metadata_frame = metadata_ciphertext || metadata_tag        (tag: 16 bytes)
metadata_frame_len (M) = len(metadata_plaintext) + 16

key   = metadata_key      (Section 5.4)
nonce = 12 zero bytes
aad   = empty byte string
```

The zero nonce is safe if and only if no two *distinct* metadata plaintexts
are ever encrypted under one `metadata_key`. The salt derivation makes this
structural: the metadata plaintext is itself an input to the salt
(Section 5.4.1), so a different metadata plaintext — even for the same object
and content — yields a different salt, header hash, and `metadata_key`. Keys
repeat only when identifier, content, *and* metadata all repeat, i.e. only
when the zero nonce re-encrypts the byte-identical plaintext, which is not
nonce reuse in the harmful sense (Section 12.1). The zero nonce is
byte-identical to the nonce of a non-final payload chunk 0; the
metadata/payload key separation is what prevents that from being nonce reuse
(Section 12.2). The AAD is empty because the header is already bound through
key derivation (Section 5.4 property 1).

#### 5.5.3. Metadata Schema

The top-level metadata item MUST be a map whose keys are all unsigned integers
(`InvalidCborEncoding` otherwise). Required keys:

| Key | Name | Type | Constraint |
| ---: | --- | --- | --- |
| `0` | `metadata_version` | unsigned | MUST be 1. |
| `1` | `plaintext_size` | unsigned | Length `P` of the canonical plaintext object in bytes. MUST be a positive exact multiple of the header `chunk_size` (Section 5.6.1). |
| `2` | `plaintext_digest_alg` | text | MUST be `sha256`. |
| `3` | `plaintext_digest` | bytes | Exactly 32 bytes: SHA-256 of the canonical plaintext object (Section 3.3). |

Missing required keys → `MissingRequiredMetadataField`; wrong type or value —
including `metadata_version` ≠ 1, a `plaintext_size` of zero or not a multiple
of `chunk_size`, a digest of the wrong length, or a `plaintext_size` for which
any Section 5.6 derived quantity overflows — → `InvalidMetadataField`.

Version 1 defines **no optional keys**: a conformant writer MUST emit exactly
the four required keys and nothing else. The metadata plaintext is therefore a
deterministic function of the canonical object — a property the zero-nonce
argument and reseal determinism rely on (Sections 5.4.1, 5.5.2, 12.1).
Descriptive object metadata is redundant here: the object identifier lives in
the plaintext header, the payload type is fixed by this specification, the
write timestamp lives in the canonical global header, and application metadata
belongs in the manifest's reserved containers — inside the canonical,
encrypted, digest-covered bytes, where it cannot perturb the envelope.

Unknown top-level unsigned integer keys remain the 1.x read-side extension
surface: readers MUST NOT reject metadata for containing them, MUST NOT let
them alter parsing or interpretation, and SHOULD preserve them bytewise when
re-emitting. (A 1.x writer extension inherits zero-nonce safety structurally,
because the metadata bytes are bound into the salt derivation —
Section 5.4.1.) A defined key with the wrong type or value →
`InvalidMetadataField`. Any future change giving a metadata key
payload-interpretation semantics requires a new magic.

The metadata frame is intentionally small — the object's *index* is the
manifest inside the encrypted payload, not the metadata frame. Frame bounds:
`17 ≤ M ≤ RAO_MAX_METADATA_FRAME_LEN`; readers MUST reject violations
(`MetadataFrameLengthInvalid`). (Lower bound: one ciphertext byte plus the
16-byte tag; every schema-valid frame is larger.)

### 5.6. The Payload Frame

The payload frame begins at `RAO_HEADER_LEN + M = 128 + M` and contains the
canonical plaintext object encrypted with the age-style STREAM construction
[AGE] — ChaCha20-Poly1305, counter nonces, an authenticated final chunk —
with the AEAD plaintext chunk size set to the object's `chunk_size` (`C`).
Sections 5.6.1–5.6.3 specify the construction completely; [AGE] is cited as
informative provenance only.

#### 5.6.1. Chunking

Let `P = plaintext_size`. Because `P` is a positive exact multiple of `C`
(Sections 3.1, 5.5.3):

```text
chunk_count = P / C
```

- Every plaintext chunk is **exactly `C` bytes** — there is no short final
  chunk and no empty-payload case (a canonical object always contains at least
  the global header, manifest, and EOF).
- Plaintext chunk `i` is exactly inner body block `i`: the AEAD chunk
  boundaries coincide one-to-one with the canonical object's `BodyLba` grid.
  This identity is the design point that preserves partial file restore on
  ciphertext (Section 6.3).
- The final chunk (index `chunk_count − 1`) is marked final. Decryption MUST
  fail if EOF is reached before an authenticated final chunk has been
  processed (`MissingFinalChunk`).

#### 5.6.2. Chunk Structure and Nonces

Each stored chunk is `chunk_ciphertext || chunk_tag` (tag: 16 bytes), so each
stored chunk occupies exactly `C + 16` bytes. For chunk `i`:

```text
key   = payload_key       (Section 5.4)
nonce = 11-byte big-endian chunk counter (= i) || final_flag
aad   = empty byte string
final_flag = 0x01 if i == chunk_count − 1, else 0x00
```

The counter starts at zero and increments by one per chunk. Since `P < 2^64`
and `C ≥ 512`, the counter never exceeds 2^55; implementations MAY hold it in
64 bits, big-endian-encoded into the low 8 bytes of the 11-byte field with the
high 3 bytes zero. A reader MUST verify each chunk's tag — decrypting with the
nonce determined by the chunk's index and *computed* finality — before
releasing, hashing, or parsing that chunk's plaintext. Finality is always
computed from `chunk_count` (that is, from the authenticated `plaintext_size`),
never discovered by probing flag values or by position relative to EOF. The
nonce construction binds each chunk to its index under a key bound to this
object's header: reordered, duplicated, spliced, or cross-object chunks fail
authentication by construction (Section 12.3).

#### 5.6.3. Frame Length

```text
payload_frame_len = P + 16 × chunk_count = P + 16 × (P / C)
footer_offset     = 128 + M + payload_frame_len
```

All derived quantities MUST be computed with checked unsigned 64-bit
arithmetic (Section 2.4). A reader that streams chunk-by-chunk without
materializing totals is equally conformant provided each incremental
computation is checked; the impossible-size failure then surfaces as
`UnexpectedEof` or `MissingFinalChunk`.

### 5.7. Footer and Final Fill

Every encrypted object ends with the 16-byte completion footer at
`footer_offset`:

```text
ASCII "RAO1_STREAM_END."  =  52 41 4F 31 5F 53 54 52 45 41 4D 5F 45 4E 44 2E
```

followed by zero fill to the next multiple of `C`:

```text
stored_size_bytes   = roundup(footer_offset + 16, C)
stored_size_blocks  = stored_size_bytes / C
fill_len            = stored_size_bytes − (footer_offset + 16)        (0 ≤ fill_len < C)
```

The footer is a mechanical completion marker, not a cryptographic mechanism:
it detects incomplete writer success paths (Section 5.8). Readers MUST verify
the footer appears exactly at the derived `footer_offset` (`InvalidFooter` on
mismatch) and MUST NOT locate it by scanning; a byte sequence matching the
footer inside ciphertext has no meaning. The fill is the only plaintext after
the footer; Verifiers MUST confirm it is all zero and report a nonzero fill
(`FillNotZero`) — it indicates a defective writer, damage, or a covert
channel, even though it cannot affect payload recovery. For a whole-object
input, any bytes beyond `stored_size_bytes` → `TrailingData`.

The fill is **inside** the stored bytes: it is covered by `stored_digest` and
by tape parity (Section 9), and it is what makes the stored copy an exact
multiple of `C` on every backend (byte-stable fanout, Section 8.1), and the
whole geometry keyless-derivable (Section 5.10).

### 5.8. Sealing

Sealing wraps a canonical plaintext object byte string. The Sealer's inputs
are: the canonical bytes (as a stream), their length `P`, their SHA-256
`plaintext_digest`, the `chunk_size` `C`, the `object_id`, the root key
material, and the `key_id`. `P` and `C` are known before sealing begins
(planning determinism); `plaintext_digest` MUST have been computed when the
canonical bytes were produced or staged — sealing is single-pass and the
digest is sealed into the metadata frame *before* the payload is written.

There are two conformant ways to obtain the input:

- **Build-and-seal**: the Builder produces the canonical bytes (Section 4)
  into a spool, file, or the plaintext copy being written in the same fanout,
  computing `P` and `plaintext_digest` as it emits them; the Sealer then seals
  from those bytes. The build itself enforces the caller-supplied per-file
  `file_sha256` checks of Section 4.6.5.
- **Seal-from-stored**: the Sealer reads an existing plaintext copy. The
  caller MUST supply the trusted catalog `plaintext_digest` (= `stored_digest`
  of the plaintext copy) as the expected value; full re-verification of the
  inner stream is then optional, because step 5 below proves the sealed bytes
  match the trusted digest.

Workflow (normative):

1. Validate inputs: `C` a positive multiple of 512; `P` a positive multiple of
   `C`; `object_id` 1–64 bytes of NUL-free UTF-8; root key ≥ 32 bytes;
   `key_id` nonzero.
2. Serialize and validate the metadata (Section 5.5), yielding `M`.
3. Derive the salt (Section 5.4.1), construct the **final** header using `M`,
   and derive keys from the hash of that final header. A Sealer MUST NOT
   derive keys from a placeholder header and backfill: a backfilled header
   silently changes `header_hash` after keys were derived, producing an
   unreadable object.
4. Write the header and the metadata frame; then stream the canonical bytes
   through chunked encryption (Section 5.6), writing each stored chunk.
5. While consuming the input, independently compute the byte count and SHA-256
   of the bytes actually read. On completion, if the computed size differs
   from `P` → fail with `PlaintextSizeMismatch`; if the computed digest
   differs from `plaintext_digest` → fail with `PlaintextDigestMismatch`. On
   any failure the Sealer MUST NOT write the footer; an object missing its
   footer is incomplete by definition and MUST NOT be treated as sealed or
   referenced by any durable catalog.
6. On success, write the footer and the zero fill, and report the computed
   `stored_digest` (over the complete stored bytes, fill included),
   `stored_size_bytes`, `stored_size_blocks`, and the envelope geometry (`M`)
   to the caller for cataloging.

Step 5 is mandatory even when the same component computed the digest minutes
earlier: it proves the Sealer sealed the bytes it was given, not the bytes the
metadata describes. Commit semantics are binding-specific (Section 8):
temp-file + fsync + rename for file outputs; the parity layer's durable
object-commit operation for tape ([REMPARITY]).

### 5.9. Opening and Verification (Keyed)

A Reader or Verifier processing a whole encrypted object MUST:

1. Read and validate the 128-byte header (Section 5.2).
2. Resolve the root key via `key_id`; derive keys (Section 5.4).
3. Read exactly `M` bytes; authenticate and decrypt the metadata frame
   (`AeadAuthenticationFailed` on tag failure); validate the metadata CBOR and
   the schema (Section 5.5).
4. Recompute the expected `hkdf_salt` per Section 5.4.1 — the root key, the
   header `object_id_field`, the metadata `plaintext_digest`, and the SHA-256
   of the decrypted metadata plaintext are all now in hand — and reject
   disagreement with the header salt (`SaltDerivationMismatch`). This promotes
   the Section 5.4.1 derivation from writer policy to a verified property of
   every readable object.
5. Compute `chunk_count = P / C` and the expected frame geometry
   (Section 5.6.3).
6. Authenticate and decrypt exactly `chunk_count` chunks in order, verifying
   each tag before its plaintext is released, hashed, or parsed. While
   streaming, compute SHA-256 and the byte count of the recovered plaintext.
7. Verify the recovered size and digest against `plaintext_size` and
   `plaintext_digest` (`PlaintextSizeMismatch` / `PlaintextDigestMismatch`).
   Although chunk authentication already guarantees integrity under the
   derived key, this check additionally proves the *sealer* matched metadata
   to payload, and it detects key-holder forgeries in which authenticated
   metadata deliberately misstates the digest.
8. Verify the footer at `footer_offset` and the zero fill; for whole-object
   inputs, confirm the input ends at `stored_size_bytes` (Section 5.7).
9. Deliver the recovered canonical plaintext object to a plaintext-stream
   Reader (Section 4.9), which applies all of its acceptance rules — in
   particular the global-header gates — plus these envelope cross-checks
   (`InnerObjectMismatch` on failure):
   - inner `REMANENCE.object_id` = header `object_id`;
   - inner `REMANENCE.chunk_size` = header `chunk_size`;
   - inner `REMANENCE.format_id` = `rao-v1` and `REMANENCE.encryption` =
     `none` (these are the Section 4.5.2 gates; their failure inside a
     well-authenticated envelope indicates a defective sealer).

Steps 6–9 MAY be pipelined: chunk plaintext can flow into the inner Reader as
chunks authenticate. Every byte released downstream has been authenticated at
chunk granularity, so a failure mid-stream leaves the consumer with an
authentic prefix of the true plaintext — but consumers MUST NOT treat streamed
output as complete or valid until the open reports overall success, and
restore-mode per-file digest verification (Section 4.9 step 5) applies to the
inner entries as always.

**Fail-closed rule.** On any AEAD tag failure, the implementation MUST stop,
MUST NOT release that chunk's plaintext, and MUST NOT retry with different
parameters (other finality, other index, other key). There is no salvage mode
across a failed tag: recovery of a damaged encrypted object is the parity
layer's job on the stored bytes (Section 9), after which decryption is retried
on the recovered ciphertext. (The plaintext-stream salvage mode of Section 4.9
remains available for the *decrypted inner stream* — authenticated bytes whose
inner structure is damaged indicate a defective writer, and salvaging them is
a deliberate, explicitly-labeled reader mode as ever.)

### 5.10. Keyless Inspection and Verification

The envelope frames its payload rigidly enough that no key is needed to
recover the object's geometry: for a whole-object input of total length `S`,
candidate chunk counts are spaced `C + 16` stored bytes apart while the
final-block fill absorbs at most `C − 1`, so **exactly one** chunk count is
consistent with any valid `S`:

```text
chunk_count    = floor((S − 144 − M) / (C + 16))
plaintext_size = chunk_count × C
footer_offset  = 128 + M + chunk_count × (C + 16)
```

(144 = header + footer; `M` = `metadata_frame_len`; `C` = `chunk_size`.)

A Keyless Verifier processing an encrypted object without key material:

- MUST validate the plaintext header (Section 5.2);
- MUST read the declared metadata frame (`UnexpectedEof` if incomplete);
- MUST compute `stored_digest` over all input bytes consumed, to EOF;
- MUST derive the geometry as above and reject an input that admits no
  consistent geometry — `chunk_count` < 1, `S` not a multiple of `C`, or
  `roundup(footer_offset + 16, C) ≠ S` — then verify the footer bytes at the
  derived `footer_offset` (`InvalidFooter`) and the all-zero fill
  (`FillNotZero`). **Keyless error classification is advisory**: the
  derivation consumes the input length itself, so truncated and appended bytes
  shift the derived `chunk_count` rather than presenting as surplus. (Appending
  one all-zero block derives a self-consistent geometry one chunk larger whose
  footer check then fails at the wrong offset — `InvalidFooter`, not
  `TrailingData`; appending a sub-block amount is caught only as a
  non-block-multiple length.) A keyless verifier MAY report `UnexpectedEof` or
  `TrailingData` for any inconsistent length; the exact Section 5.7
  classification belongs to keyed readers, which learn the true
  `plaintext_size` from authenticated metadata, and to verifiers holding a
  catalog-recorded stored length or `stored_digest`;
- MUST NOT claim authentication: a deliberately extended input can present a
  self-consistent geometry, a well-placed footer, and clean fill — the derived
  geometry and footer prove framing and completeness only, and the computed
  `stored_digest` is meaningful solely against a trusted catalog value.

Keyed readers MAY cross-check the derived `chunk_count` against the decrypted
`plaintext_size`; a mismatch in a tag-valid object is impossible by
construction, so a disagreement indicates an arithmetic defect, not damage.

A keyless **inspect** operation reveals
exactly the header fields: magic, version, suite, `chunk_size`, `key_id`,
`hkdf_salt`, `metadata_frame_len`, `object_id` — plus the stored length and
the derived `plaintext_size`/`chunk_count` above. It reveals **no** filenames,
member sizes, file count, or manifest content. Keyless inspection is how an operator
recovers *which* key epoch to materialize (`key_id` → registry) without
holding any key.

## 6. Partial File Restore

PFR maps a member-file byte range to stored byte ranges by closed-form
arithmetic. The per-file index — `first_chunk_lba` (an inner `BodyLba`) and
`size_bytes` per file, from the manifest or the catalog — is the **same for
both representations** of an object, because both wrap the same canonical
bytes. Catalog per-file rows therefore need to be stored once per object, not
per copy. Restorers MUST treat plaintext offsets (inner `BodyLba`, file byte
ranges) as the source of truth and MUST NOT make ciphertext offsets canonical;
stored offsets are derived, reproducible from this section.

**Hardlinks.** A hardlink entry has `size_bytes = 0` and `first_chunk_lba`
`null` (Section 4.7.2); it stores none of its own content. PFR on a hardlinked
name MUST first resolve its `link_target` to the primary entry and then use the
**primary's** `first_chunk_lba` and `size_bytes` for all arithmetic below. A
PFR implementation MUST NOT treat a hardlinked name as an empty or invalid
range. (Symlinks and directories carry no payload and are not PFR targets.)

A Restorer working from a per-file index (catalog rows rather than the full
manifest) MUST preserve the ability to resolve a hardlink: the hardlink's row
MUST carry `entry_type` + `link_target` (resolved at restore time), **or** a
denormalized pointer to the primary's `first_chunk_lba`/`size_bytes`. An
index that stores only the literal `first_chunk_lba`/`size_bytes` of a
hardlink row (`null`/`0`) cannot support conformant restore of that name.

### 6.1. Range Validation

Given a file with size `Z` and a requested range `[s, s + n)`: if `n = 0` the
result is the empty range set. Otherwise the Restorer MUST validate
`s + n ≤ Z` with checked arithmetic before applying any formula below; the
formulas are defined only for validated, non-empty ranges.

### 6.2. Inner Mapping (Both Representations)

With `C = chunk_size`, `L = first_chunk_lba`:

```text
b_first = L + floor(s / C)
b_last  = L + floor((s + n − 1) / C)
```

File byte `x` lives in inner body block `L + floor(x / C)` at offset
`x mod C` (file payloads start block-aligned, Section 4.6.3). The requested
bytes are obtained from inner blocks `b_first ..= b_last` with head/tail
trimming; the final block of a file holds `Z − (chunk_count − 1) × C` payload
bytes, with unrelated stream bytes after them (Section 4.6.4) — trim by `Z`,
never by block boundaries. For a **plaintext copy** this is the whole
computation: inner blocks are stored blocks; read them and trim.

### 6.3. Ciphertext Mapping (Encrypted Representation)

Inner body block `b` is AEAD chunk `b` (Section 5.6.1). With
`F = 128 + metadata_frame_len`:

```text
cipher_offset(b) = F + b × (C + 16)
cipher_len       = C + 16
nonce counter    = b
final_flag       = 0x01 if b == chunk_count − 1, else 0x00
```

The Restorer fetches each stored range `[cipher_offset(b), cipher_offset(b) +
C + 16)` for `b` in `b_first ..= b_last` (a single contiguous stored range,
since consecutive chunks are adjacent), authenticates and decrypts each chunk
with the nonce above, concatenates the plaintexts, and slices the requested
bytes per Section 6.2. Finality comes from the formula — that is, from the
authenticated `plaintext_size` — never from probing. A Restorer MUST NOT
release plaintext from a chunk whose tag failed (Section 5.9). `chunk_count`
and `F` require the decrypted metadata frame; PFR on an encrypted copy
therefore requires the key by construction, plus one metadata-frame read. The
per-file row comes from the catalog; with no catalog, recovery of an encrypted
copy decrypts the inner stream sequentially until the manifest — the final
entry — is parsed, because encrypted bootstrap rows deliberately carry no
manifest anchor (Section 8.2); PFR against the freshly recovered manifest then
proceeds by this same mapping.

### 6.4. Stored-Block Mapping (Tape and Block-Addressed Backends)

On a byte-addressed backend (file, object store with range reads), the
Section 6.3 stored byte range is fetched directly. On tape, the stored copy is
one tape file of fixed blocks of size `C` (Section 8.2), addressed by stored
`BodyLba`. A stored byte range `[a, a + l)` maps to stored blocks:

```text
first_stored_block = floor(a / C)
last_stored_block  = floor((a + l − 1) / C)
```

Because each stored chunk is `C + 16` bytes, the ciphertext of one inner block
spans at most `ceil((C + 16) / C) + 1 = 3` consecutive stored blocks, and a
run of `k` consecutive chunks occupies one contiguous stored-block range of at
most `k + ceil(16 × k / C) + 1` blocks — `k + 2` whenever `16 × k ≤ C` (the
16-byte-per-chunk tag slip is why stored `BodyLba` ≠
inner `BodyLba` for encrypted copies). This bounded, contiguous read
amplification is the accepted cost of keeping the stored stream block-uniform;
the rationale and the rejected alternative are in Appendix B.

## 7. Digests, Integrity, and the Verification Chain

### 7.1. The Chain of Trust

```text
off-tape catalog                      on-tape parity-layer bootstrap row
(stored_digest + plaintext_digest     (plaintext copies: manifest location +
 per copy — Section 12.5 trust         manifest_sha256; encrypted copies:
 domain)                               the Section 8.2 envelope fields only)
        │  externally anchored; the bootstrap is parity-protected on tape
        ▼
[encrypted copies] envelope: header-bound derived keys → authenticated metadata frame
        │            (plaintext_size, plaintext_digest) → per-chunk Poly1305 tags
        ▼
canonical plaintext object ── byte-verified by plaintext_digest
        │
        ▼
manifest.cbor ── byte-verified by manifest_sha256
        │
        ▼
per-file  file_sha256, size_bytes, first_chunk_lba, chunk_count (regular entries;
          a hardlink resolves via link_target to its primary's fields)
        │
        ▼
payload bytes ── byte-verified by file_sha256
```

The pax `REMANENCE.file_sha256` keywords duplicate the manifest hashes as a
within-stream cross-check. For an encrypted copy, the decrypted manifest's
anchor is the authenticated `plaintext_digest`, which covers every canonical
byte including the manifest; the Section 4.7.2 anchor-digest obligation is
satisfied by it (with the manifest entry's own pax `REMANENCE.file_sha256` as
the within-stream self-consistency check) — external manifest anchors exist
for plaintext copies only (Section 8.2). What the chain does **not** provide is
covered in Sections 12.6 (no self-authentication of plaintext copies) and 12.7
(non-committing AEAD).

### 7.2. Write-Path Verification (No Extra Reads)

Every digest in the chain is computed over bytes already flowing through the
writer — the chain costs hash arithmetic, never an additional read pass:

1. **Per-file, at build.** The Builder streams each payload file, hashing it,
   and MUST fail the object if the streamed SHA-256 or byte count differs from
   the caller-supplied expected `file_sha256`/size (Sections 4.6.5, 4.9). This
   proves the writer archived the payload it was given, not the payload the
   metadata describes. A failed object MUST NOT be completed or reported as
   complete.
2. **Canonical stream, at build.** The Builder computes `plaintext_digest`
   (= the plaintext copy's `stored_digest`) over its own emitted byte stream,
   and reports it with the layout for cataloging.
3. **Envelope, at seal.** The Sealer recomputes size and digest of the bytes
   actually sealed and fails — footer unwritten — on mismatch (Section 5.8
   step 5). It computes the encrypted copy's `stored_digest` over its own
   emitted bytes.

### 7.3. Post-Write Re-Verification (Deployment Obligation)

After each copy is written, and before that copy is recorded durable, the
deployment is expected to re-read the copy via the object read path and
re-verify it —
for a full verification, every **regular** member's `file_sha256` plus the
Section 7.4 non-regular correspondence and hardlink referential checks (which
transitively exercise the envelope on encrypted copies); at minimum, the copy's
`stored_digest`. This is a media/transmission guard, deliberately distinct
from the Section 7.2 build checks: it is the one intentional extra read in the
pipeline, and it is a *deployment* (workflow) obligation rather than a property
of the bytes — a conformant Verifier (Section 7.4) is the tool that discharges
it.

### 7.4. Verifier Profile

A Verifier validates one stored copy end to end without extracting it:

- **Plaintext copy**: the full restore-mode read of Section 4.9 with every
  regular entry's digest checked, manifest anchor-digest and schema validation
  (Section 4.7.2), manifest-vs-archive correspondence (every member entry
  appears in `file_entries` with matching `path` and `entry_type`; **regular
  entries** match `size_bytes`, `file_sha256`, `first_chunk_lba`, and
  `chunk_count`; **hardlink entries** match `link_target`, carry zero/`null`
  content fields and no `file_sha256`, and resolve to a valid regular-file
  primary (Section 4.6); **symlink/directory entries** carry zero/`null`
  content fields; and `file_entries` lists nothing absent from the archive),
  final-fill zero check (Section 4.8), and
  report-all-nonconformities (not first-error-only), plus a `stored_digest`
  comparison against the catalog value when available.
- **Encrypted copy (keyed)**: Section 5.9 in full (header, metadata, salt
  derivation, every chunk tag, plaintext size+digest, footer, fill, inner
  cross-checks), with the recovered inner stream verified as above, plus the
  `stored_digest` comparison.
- **Encrypted copy (keyless)**: Section 5.10 — structure plus `stored_digest`;
  explicitly not an authentication claim.

A successful keyed verification means: every payload byte hashes to its
declared identity, the object's self-description is complete and consistent,
and (encrypted) every stored chunk authenticates under the object's derived
keys.

### 7.5. Scrub

Backends scrub stored copies by `stored_digest` (whole-copy) without keys. On
tape, the parity layer additionally CRCs every stored block and can verify and
repair at block granularity without reading the whole object (Section 9). Both
operate on stored bytes and are representation-agnostic.

## 8. Storage Bindings and Backend Independence

### 8.1. The Byte-Format Contract

An RAO object in either representation is a byte string. Any conformant tool
can produce it; any backend can store it; `stored_digest` is computed over the
identical bytes everywhere ("byte-stable fanout"). A backend needs no keys, no
plaintext access, and no format knowledge to store, replicate, compare, or
scrub a copy. Backends SHOULD record per copy: location, representation,
`stored_digest`, `stored_size_bytes`/block count, `chunk_size`, and — for
encrypted copies — `key_id` and `metadata_frame_len` (the latter so the PFR
arithmetic of Section 6.3 needs no header read).

### 8.2. Tape Binding

The stored bytes are written as one tape file of fixed-size tape blocks,
terminated by a filemark written by the parity layer at object close. The tape
block size MUST equal the object's `chunk_size`, for both representations —
one stored block is one tape block, stored `BodyLba` is the tape file's block
index, and parity geometry is uniform. Parity sidecars, the filemark map,
block CRCs, and the BOT bootstrap are tape-binding artifacts owned by the
parity layer (Section 9); they exist **only** on tape and are not part of the
object's stored bytes on any backend.

Bootstrap rows differ by representation, deliberately. A **plaintext**
object's row carries the manifest anchors (`manifest_first_chunk_lba` — an
inner `BodyLba` — `manifest_size_bytes`, `manifest_chunk_count`,
`manifest_sha256`). An **encrypted** object's row carries no manifest
anchors — only the envelope fields (representation, `key_id`,
`metadata_frame_len`, stored block count); [REMPARITY] states this as a
normative rule of its bootstrap rows, and a Writer producing the tape
binding MUST honor it. Manifest size and location are
structural facts about confidential content — manifest size correlates
directly with member count — and the bootstrap is plaintext on the very tape
the envelope protects; the envelope's authenticated metadata already anchors
the manifest more strongly than an external digest could (Section 7.1).
The REM-PARITY binding encodes these rows in bootstrap CBOR key 30 as
specified by the parity format: plaintext rows use keys 10–13 for manifest
anchors, while encrypted rows use keys 20–21 for `key_id` and
`metadata_frame_len`; the two sets are mutually exclusive.
Catalog-less recovery from tape: keyless, an operator recovers each encrypted
object's `object_id` and `key_id` from stored block 0 (the header); with the
key, the full self-describing object — the manifest located by sequential
decryption rather than by anchor, an acceptable cost on a recovery path over
sequential media. Plaintext objects remain fully recoverable keyless
(Section 4.10).

### 8.3. File Binding

The stored bytes as one regular file; RECOMMENDED extension `.rao` for both
representations (Section 3.4 disambiguates). Writers MUST follow a durable
commitment protocol: write to an exclusively-created temporary path (e.g.
`name.rao.partial`), flush and fsync the file before renaming to the final
path, and fsync the containing directory before reporting success. A rename
without prior synchronization can leave the final name referring to
incompletely persisted data after a crash. Partial outputs SHOULD be deleted
or quarantined and MUST NOT be referenced by any durable catalog.

### 8.4. Object-Store Binding

The stored bytes as one object/blob, `stored_digest` recorded as integrity
metadata, uploaded with whatever integrity the store offers (e.g. checksum
headers), and verified by digest after upload (Section 7.3). Ranged reads
(Section 6.3) make PFR efficient without downloading whole objects. The
encrypted representation is the intended cloud copy; storing plaintext copies
on shared infrastructure is a deployment policy question, not a format one.

## 9. Relationship to the Parity Layer

The parity layer [REMPARITY] protects tape-resident stored blocks with
Reed-Solomon parity, block CRCs, parity-epoch sidecar tape files, a filemark
map, and the replicated BOT bootstrap. **The parity construction and geometry
are independent of this document**; what RAO relies on, and what it adds to
the bootstrap, is:

1. **Parity is computed over stored bytes** — the ciphertext, when the copy is
   encrypted. The order is: build → (seal) → parity. The parity layer protects
   bytes regardless of content and needs no keys, ever; recovery of damaged
   blocks of an encrypted object proceeds keyless, after which decryption is
   retried on the recovered stored bytes (Section 5.9 fail-closed rule).
2. Within one object's tape file there are no parity or bootstrap blocks; the
   object's stored blocks are contiguous (stored `BodyLba` 0..N−1). Parity
   epochs span objects; sidecars land between tape files. None of this is
   visible in, or part of, the object's stored bytes.
3. An object is *committed* when the parity layer's durable object-commit
   operation completes ([REMPARITY]); neither representation defines an
   in-band commit marker (the envelope footer detects incomplete writes; it
   is not a commit barrier).
4. The bootstrap row carries, per encrypted object, the envelope fields of
   Section 8.2 (representation marker, `key_id`, `metadata_frame_len`, stored
   block count) and never manifest anchors. This is an additive bootstrap
   schema requirement, not a change to the parity construction.

## 10. Versioning and Extensibility

RAO version 1 is identified by a **pair** of wire identifiers that version
together:

- **Plaintext stream**: `REMANENCE.format_id = rao-v1`,
  `REMANENCE.schema_version = 1.<minor>`. Minor revisions are additive — new
  `REMANENCE.*` pax keywords (global or per-entry), new top-level or per-entry
  manifest keys, and content inside the reserved `object_metadata`,
  `external_references`, and `metadata_preservation_data` containers (the
  designated landing zone for xattrs and ownership tiers; hardlinks are already
  native 1.0 entries, Section 4.6) — and MUST NOT change any rule a 1.0 Reader
  enforces. A 1.0
  Reader gates on the major version and tolerates all of the above
  (Sections 4.4.3, 4.7.2). Any change that could make a 1.0 Reader
  misinterpret bytes (new entry kinds with payload semantics, alignment
  changes, manifest re-keying, compression, encryption flagged inside the
  stream) MUST be made under a new `format_id` — which is, by definition, RAO version 2.
  In particular, `REMANENCE.compression` and `REMANENCE.encryption` are
  permanently `none` in every conformant 1.x stream (the refusal gates of
  Sections 4.5.2 and 4.6.2): introducing a real value for either is a
  new-`format_id` change, never a 1.x one.
- **Envelope**: magic `RAO1`, `format_version` 1, `suite_id 0x01` — all
  frozen (Section 5.2). Any change to envelope parsing, cryptography, chunk
  framing, or metadata semantics requires a new magic (`RAO2`). There is no
  in-place agility: no negotiable suites, no version ranges. New *metadata*
  keys are the one extension surface (Section 5.5.3), and they MUST remain
  descriptive. This is a deliberate trade — every valid `RAO1` envelope is
  parseable by every conformant reader forever.

A future plaintext-stream break and a future envelope break each produce RAO
version 2; this document's successor would then specify the new pair.

Native symbolic links, hardlinks, and empty directories are included in version 1 as a
pre-freeze scope expansion: no RAO 1.0 implementation had been published or
deployed when they were added. After freeze, adding new entry kinds with
payload semantics or changing zero-payload alignment requires a new
`format_id`.

## 11. Errors

Implementations SHOULD expose typed errors equivalent to the taxonomy below.
Names are normative for the test-vector manifests (Section 13); surface syntax
is not. I/O failures MUST remain distinguishable from format violations so
callers can tell storage problems from invalid objects. Code paths reachable
from object bytes MUST NOT panic, crash, or allocate unboundedly
(Section 12.9).

### 11.1. Plaintext-Stream Errors

These apply to plaintext copies and to the decrypted inner stream of encrypted
copies alike.

```text
InvalidInput              caller-supplied object/file metadata violates Section 4.6.6 / 4.9
Layout                    layout arithmetic overflowed or an invariant could not be satisfied
Parse                     malformed archive structure (octal fields, EOF sequence, missing
                          required pax keys, short blocks, entry after the manifest,
                          truncated/overflowing offsets)
UstarChecksumMismatch     Section 4.3.3 failure
UnsupportedTarTypeflag    Section 4.3.4 rejection
InvalidHardlinkTarget     hardlink target absent, not a regular-file primary, or not preceding the link (4.6)
ChunkAlignmentViolation   Section 4.6.3 reader rejection
ChunkSizeMismatch         stream REMANENCE.chunk_size disagrees with supplied geometry (4.5.2)
InvalidPath               effective path violates Section 4.6.6 rules 1-2 (reader-side)
TruncatedPayload          EOF inside declared payload, pax body, or padding
PaxRecordMalformed        Section 4.4.1 grammar violation
FileDigestMismatch        delivered payload bytes do not hash to REMANENCE.file_sha256 (4.9)
Cbor                      manifest is not valid manifest-profile CBOR (Section 4.7.1)
ManifestInvalid           manifest violates the Section 4.7.2 schema or cross-checks
ManifestDigestMismatch    manifest bytes do not hash to the anchor digest (Section 4.7.2)
UnsupportedFeature        unknown format_id, schema major mismatch, non-none compression
                          or encryption
IncompleteBlockWrite      Section 4.9 writer failure
SourceIo                  payload source read failure (not a format violation)
TapeIo                    block sink/source failure (not a format violation)
```

### 11.2. Envelope Errors (Encrypted Representation)

```text
InvalidMagicBytes            input does not begin with RAO1
InvalidHeaderLength          header_len field is not 128
UnsupportedFormatVersion     format_version is not 1
InvalidSuite                 suite_id is not 0x01
InvalidChunkSize             chunk_size is zero or not a multiple of 512
ReservedBytesNotZero         flags or reserved bytes are nonzero
InvalidKeyIdentifier         all-zero key_id, or key_id unknown to the resolver
InvalidSalt                  all-zero hkdf_salt
SaltDerivationMismatch       header hkdf_salt differs from the Section 5.4.1 derivation
                             (keyed open/verify, Section 5.9 step 4)
InvalidObjectIdField         object_id field all-NUL, interior NUL, or invalid
                             UTF-8 (reader-side; a >64-byte object_id is
                             rejected at sealing time as InvalidInput, 5.2)
MetadataFrameLengthInvalid   metadata_frame_len outside [17, 16 MiB]
InvalidRootKey               root key material shorter than 32 bytes
UnexpectedEof                declared header, metadata frame, footer, or fill bytes missing
MissingFinalChunk            EOF within the payload frame before an authenticated final chunk
AeadAuthenticationFailed     metadata or chunk tag verification failed
InvalidCborEncoding          metadata frame plaintext is not valid metadata-profile CBOR (5.5.1)
MissingRequiredMetadataField required metadata key absent
InvalidMetadataField         metadata key wrong type/value; plaintext_size zero, not a
                             multiple of chunk_size, or implying overflow
PlaintextDigestMismatch      computed canonical-bytes digest differs from plaintext_digest
PlaintextSizeMismatch        computed canonical-bytes size differs from plaintext_size
InvalidFooter                bytes at footer_offset are not the footer
FillNotZero                  nonzero byte in the post-footer fill
TrailingData                 bytes beyond stored_size_bytes in a whole-object input
InnerObjectMismatch          decrypted stream's object_id / chunk_size / format gates
                             disagree with the envelope header (Section 5.9 step 9)
InvalidInput                 sealing input violates Section 5.8 step 1 (writer-side)
Io                           underlying I/O failure that is not a format violation
```

The recommended detection orders are Section 5.2 (header) and Section 5.9
(whole object). For multi-fault inputs any applicable error is conformant;
test vectors are single-fault by construction (Section 13).

## 12. Security Considerations

### 12.1. Per-Object Key Uniqueness Is Structural

The catastrophic failure of this construction would be two objects under one
root key deriving identical `metadata_key`/`payload_key` for *different*
plaintexts: the zero metadata nonce and every payload chunk-index nonce would
then be reused across distinct contents — for ChaCha20-Poly1305 a total
failure (XOR of plaintexts leaks immediately; Poly1305 key recovery enables
forgery). RAO closes every operational path to that state by two independent
mechanisms, leaving only the quantified residual stated below:

1. **Header binding.** Keys derive from `header_hash`, and the header contains
   the 64-byte `object_id`: objects with distinct identifiers derive distinct
   keys *regardless of their salts*.
2. **Salt derivation.** The salt is a PRF of the object's identifier, its
   content digest, *and* its metadata bytes (Section 5.4.1): objects sharing
   an identifier but differing in content or metadata derive distinct salts,
   hence distinct headers, hence distinct keys — and the derivation is
   re-verified on every keyed open (Section 5.9 step 4), so a defective sealer
   cannot produce a readable object that violates it.

Every residual path to identical keys over distinct plaintexts begins with an
`object_id` reused across seals that differ in content or in envelope metadata
(a defect in the identifier-assigning system or the sealer — identifiers are
required to be unique by whoever assigns them;
v1 metadata cannot differ for one object, Section 5.5.3, but a 1.x metadata
extension can, which is why the model covers it); for distinct identifiers,
header binding ends the analysis. Given such a reuse, keys collide only if one
of two further things happens:

1. **A SHA-256 collision in the digest inputs.** The derivation sees content
   only through `plaintext_digest` and metadata only through `metadata_hash`,
   so two distinct same-size canonical objects with colliding content
   digests — or two distinct metadata plaintexts with colliding hashes —
   would derive identical salts, headers, and keys outright. This is a
   collision-resistance break of SHA-256 — an assumption the format already
   stakes its entire integrity chain on (`file_sha256`, `plaintext_digest`,
   `manifest_sha256`; Section 7.1) — so the residual model adds no primitive
   assumption the format did not already carry.
2. **A truncated-PRF output collision.** Distinct derivation inputs can still
   collide in the 16-byte salt with probability 2^−128 per pair — a truncation
   bound, not a SHA-256 or HMAC break (128 bits of salt cannot promise more),
   and accidental-only: the PRF is keyed by `root_key`, so no party without
   the root key can compute — let alone search for — salt values, and a party
   holding the root key is already inside the trust boundary (Section 12.7).

Sealing consumes no randomness, so random-number-generator (RNG) quality *during sealing* is not a
confidentiality dependency; the root key itself, generated once in the
external registry, still requires high-quality randomness (Section 5.3) — that
is where the format's reliance on entropy begins and ends. What *can* recur is
the benign case: resealing the identical canonical object (identical metadata
included — in version 1 the metadata is a function of the object,
Section 5.5.3) reproduces identical keys with the identical plaintexts —
deterministic encryption of one object, disclosing only an equality that the
header `object_id` already discloses (Section 5.4.1 property 2).

On the key-dependent Extract salt (a salt that is a PRF output under the same
`root_key` later used as the Extract `ikm`): the construction rests explicitly
on the dual-PRF assumption for HMAC-SHA-256 [RFC2104] — that it behaves as a
PRF when keyed through either input. This is the same assumption underlying
the TLS 1.3 key schedule [RFC8446], which chains key-derived Extract salts;
it is stated
here as an assumption, deliberately, and no proof is inherited.

As a defense-in-depth measure, a deployment's catalog can run a consistency
check on `(key_id, hkdf_salt)` at insert. A repeat is *legitimate* exactly when the
rows are byte-identical copies of one sealed object — agreeing on `object_id`,
`plaintext_digest`, **and** `stored_digest`: byte-stable fanout and resealing
intentionally produce such repeats (Section 5.4.1 property 2). A repeat
disagreeing on any of the three warrants loud rejection. The `stored_digest`
term is not redundant: rows agreeing on `object_id` and `plaintext_digest` but
differing in `stored_digest` are precisely the signature of residual branch 1
above (or of a defective sealer) — the one case in which `plaintext_digest`
equality cannot be trusted to mean content equality.

### 12.2. Key Separation Is Required for Nonce Safety

The metadata nonce (12 zero bytes) is byte-identical to the nonce of a
non-final payload chunk 0. The construction is safe only because
`metadata_key` ≠ `payload_key`. Any change that unifies the keys converts this
coincidence into real nonce reuse. A future revision MUST NOT merge the
metadata and payload keys.

### 12.3. Binding Without AAD

Both AEADs use empty AAD. Object identity and chunk position are bound
structurally: the object (including its `object_id`, `key_id`, salt, and
geometry) is bound through `header_hash` in the key derivation, and the chunk
index and finality are bound through the nonce. Cross-object splicing fails
because the keys differ; intra-object reordering, duplication, or truncation
fails because the nonce (index, finality) or the missing final chunk fails
authentication. Re-binding the same facts as AAD would add bytes and a second
mechanism without adding security.

### 12.4. Fail-Closed

A failed chunk or metadata tag MUST stop processing without releasing that
chunk's plaintext (Sections 5.9, 6.3); a failed seal MUST NOT produce a footer
(Section 5.8); a parity or CRC failure on stored blocks is repaired by the
parity layer before decryption is retried. Partial plaintext is never emitted
as success; streamed output is not valid until the whole-object open succeeds.

### 12.5. Confidentiality Boundary and Size Leakage

An encrypted copy reveals, by design: that it is an RAO object; the suite;
`chunk_size`; `key_id` (the key *epoch*, not the key); the salt;
`metadata_frame_len`; `object_id`; and its stored length — from which the
**exact** `plaintext_size` and `chunk_count` are derivable by the Section 5.10
arithmetic. Encryption hides content, never size. It reveals no filenames,
member sizes or count, manifest content, or payload bytes. Deployments for
which the object's existence, identifier, or approximate size is itself
sensitive must handle that above the format (padding payloads before building,
opaque object naming); RAO defines no padding mechanism.

Two adjacent systems hold facts about encrypted objects in the clear and are
separate trust domains, out of scope of the format but identified here for
completeness. The off-tape **catalog** holds paths, per-file rows, and digests —
cleartext metadata about confidential content, protected by the catalog
system, not by this format. The on-tape **parity-layer bootstrap** is
deliberately minimal: for encrypted objects it carries only the envelope
fields of Section 8.2 and no manifest anchors ([REMPARITY]) — `object_id` is
recovered from the envelope header in stored block 0, not from the bootstrap —
so the tape itself leaks nothing beyond the Section 5.10 header-and-size
facts.

### 12.6. Plaintext Copies Are Not Self-Authenticating

A plaintext RAO object provides integrity plumbing, not authentication: an
attacker who can rewrite the medium can rewrite payloads, pax hashes, and the
manifest consistently. A lone plaintext object whose hashes verify internally
proves only self-consistency. The trust anchor is external — the catalog's
`stored_digest` and, on tape, the bootstrap's parity-protected
`manifest_sha256` (plaintext rows, Section 8.2). Encrypted copies are
cryptographically authenticated under the root key — subject to Section 12.7.

### 12.7. Non-Committing AEAD

ChaCha20-Poly1305 is not key-committing: a party holding two root keys can in
principle craft one ciphertext that authenticates and decrypts to different
valid plaintexts under each [AEAD-COMMIT] [PART-ORACLE]. `stored_digest` does
not prevent this (equivocation uses one byte string). RAO's defense is
operational: the key registry is trusted, `key_id` resolution is exact, and
writers are inside the trust boundary. Deployments with mutually distrusting
writers, or a requirement that no object be interpretable under two epochs,
need a committing construction and therefore RAO version 2. Note also that the
Section 5.9 step 7 digest check detects the related key-holder forgery
(authenticated metadata misstating the digest).

### 12.8. Key Rotation and Epoch Longevity

Keys are derived per object; no wrapped data-encryption keys are stored.
Rotating the root key affects newly sealed objects only; re-keying an existing
object means resealing its canonical bytes (cheap if a plaintext copy exists —
a new seal, not a rebuild; the object and its `plaintext_digest` are
unchanged). Old key epochs need to remain recoverable in the registry for as long
as objects sealed under them are to remain readable; preserving them is a
registry/deployment obligation outside this format.

### 12.9. Hostile-Input Posture

Stored bytes come off removable media and networks and MUST be treated as
untrusted in both representations. Every parse decision is bounded: the header
is fixed-size with frozen fields; the metadata frame is length-bounded
(16 MiB) with CBOR depth/item limits enforced incrementally; payload
processing uses constant memory per chunk; all arithmetic is checked. In the
plaintext stream, the ustar header record is checksummed, pax record lengths
are validated against the remaining header payload, payload sizes are
validated against the remaining declared blocks before allocation (streaming
readers allocate O(1)), and `chunk_size`/block count arrive from the
catalog/bootstrap as semi-trusted inputs — a materializing Reader MUST use
fallible allocation and SHOULD enforce a deployment size ceiling, while
streaming Readers are immune by construction and are the production path.
Reader implementations MUST NOT panic, crash, or invoke undefined behavior on
any byte sequence, SHOULD enforce this mechanically (no `unwrap`/unchecked
indexing/unchecked arithmetic on reachable paths; forbid `unsafe` where
practical), and SHOULD validate it with coverage-guided fuzzing of the header
parser, both CBOR decoders, the record loop, and whole-object open/verify
(Section 14).

### 12.10. Path Traversal

Native entry paths cannot represent traversal: Section 4.6.6 forbids absolute
paths and `.`/`..`/empty components at write time, and Readers reject
violations (`InvalidPath`), so a conformant entry path is always a clean
relative path. Symlink targets are different: they are opaque OS strings and
may be absolute, contain `..`, or be dangling.

A Restoring Consumer (Section 2.2) MUST therefore keep its own sanitization.
It MUST NOT follow symlinks already present in the destination tree while
materializing any entry; they SHOULD use `openat`/`O_NOFOLLOW` or equivalent
component-by-component discipline and re-check each component. They MUST
create symlink entries as symlinks, without dereferencing their targets, and
MUST materialize a hardlink's primary before creating the hardlink (`link(2)`)
to the already-restored primary. They MUST also prevent the classic archive
attack where an earlier symlink entry creates `dir -> /outside` and a later
regular entry writes through `dir/file`.
Framing-layer acceptance of a path is a necessary check, not a sufficient
safety claim. Decrypting an envelope grants no exemption: the inner stream is
parsed and restored under the same rules. Stock tar extraction has its own
security model; RAO's standard-tool fallback is faithful, not inherently
sandboxed.

## 13. Test Vectors

Static test vectors are distributed alongside this specification, each with a
manifest entry recording inputs, the expected values pinned below, and — for
negative vectors — the expected Section 11 error name. Vectors use small `chunk_size`
values (e.g. 4096) so full object byte streams are practical to pin; at least
one vector MUST use `DEFAULT_CHUNK_SIZE`.

The companion archive is `remanence-test-vectors.tar`, SHA-256
`596e5ee7baffb355366407d6b4384fe7caafa64509e489508df2ed5dc2eadc7d`.
Its `MANIFEST.tsv` inventories every contained vector manifest and generated
artifact, `CHECKSUMS.sha256` authenticates them, and the included `verify.py`
checks the archive without a source checkout. The archive is reproducibly
generated with the `publication-test-vectors` build target.

Values marked **[pinned-at-generation]** are produced by the reference
implementation when the fixtures are first generated, then frozen; they cannot
be derived by arithmetic alone and this document does not guess them.
Generating and freezing them — and confirming the *derivable* expected values
stated here — is freeze criterion 2 (Section 14). All other values below are
normative now. Payload digests are independently checkable with `sha256sum`.

### 13.1. Plaintext-Stream Positive Vectors

The plaintext suite MUST include at least: an **empty object** (global header
+ manifest + EOF only); an **empty file** (`chunk_count` 0, absent
`first_chunk_lba`, `null` in the manifest); a **one-byte file**; a
**block-boundary set** (payload sizes `chunk_size − 1`, `chunk_size`,
`chunk_size + 1`, and one multi-chunk size); **pathological paths** (a
non-ASCII path and a > 100-byte path, both exercising `PAX_PATH_PLACEHOLDER`,
and a 100-byte portable path stored inline); **full metadata** (entries with
`mtime`, `executable=true` at mode 0755, and `executable` unsupplied →
`null`); a **multi-file object** ordering entries non-alphabetically (pinning
caller-order preservation); **non-regular entries** (a symlink with its
target, an empty directory, and a hardlink — primary + link — restoring to one
shared inode); **long link targets** (a symlink and a hardlink whose targets
exceed 100 bytes, exercising `PAX_LINK_PLACEHOLDER` and pax `linkpath`); and a **canonical-manifest byte-identity vector**
pinning the exact manifest CBOR bytes and `manifest_sha256` for a fixed input
set (the cross-implementation determinism gate, Section 4.7.1). For each, the
manifest pins the exact full object byte stream, or for large vectors
`full_object_sha256` plus either the first object block bytes or
`first_block_sha256`, `projected_size_blocks`, every entry's
`(pax_header_offset, data_offset, first_chunk_lba, chunk_count, pad_spaces)`,
the manifest CBOR bytes, and `manifest_sha256`.

### 13.2. RAO-TV-P1 — Plaintext Object

Inputs (complete):

| Input | Value |
| --- | --- |
| `chunk_size` | 4096 |
| `object_id` | `00000000-0000-4000-8000-000000000001` |
| `caller_object_id` | `rao-tv-1` |
| `write_timestamp` | `2026-01-01T00:00:00Z` |
| `metadata_preservation` | `minimal` |
| `manifest_file_id` | `00000000-0000-4000-8000-0000000000ff` |
| File 0 | `path` = `a/hello.txt`, `file_id` = `00000000-0000-4000-8000-000000000010`, no `mtime`, no `executable` |
| File 0 contents | The 26 ASCII bytes `hello, rem archive object` + LF |
| File 0 expected `file_sha256` | `0ea7e9ec3396345c15ef4edf44e91d8cf184feb303ba992b38d65f71dcac37e2` |
| File 1 | `path` = `b/pattern.bin`, `file_id` = `00000000-0000-4000-8000-000000000011`, no `mtime`, no `executable` |
| File 1 contents | 5000 bytes; byte `i` = `i mod 256`, `i` = 0…4999 |
| File 1 expected `file_sha256` | `8026e5c96cf1e502c8deb3e89f8b8bc342f5039b871911a92eb10edf9c6542d3` |

Expected layout (derivable; the full derivation is worked in Appendix A):

| Quantity | Expected value |
| --- | --- |
| Global header (g record + padded pax body) | bytes 0–1023 |
| File 0: pax record at offset | 1024; pad record `1812 REMANENCE.pad=` + 1792 spaces + LF |
| File 0: `first_chunk_lba`, `chunk_count` | 1, 1 (data at byte 4096) |
| File 1: pax record at offset | 4608; pad record `2320 REMANENCE.pad=` + 2300 spaces + LF |
| File 1: `first_chunk_lba`, `chunk_count` | 2, 2 (data at byte 8192) |
| Manifest: pax record at offset | 13312; manifest CBOR size 548 bytes |
| Manifest: `first_chunk_lba`, `chunk_count` | 4, 1 (data at byte 16384) |
| Tar EOF at | 17408; `total_size_bytes` = 20480; `projected_size_blocks` = 5 |

Pinned outputs: the exact full object byte stream; the manifest CBOR bytes;
`manifest_sha256` **[pinned-at-generation]**; `plaintext_digest` =
`stored_digest` **[pinned-at-generation]**.

### 13.3. RAO-TV-E1 — Encrypted Twin of RAO-TV-P1

Inputs: the RAO-TV-P1 canonical bytes, plus —

| Input | Value |
| --- | --- |
| `root_key` (32 bytes) | hex `000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f` |
| `key_id` (16 bytes) | hex `4b49443a72616f2d74762d65312e3031` (ASCII `KID:rao-tv-e1.01`) |
| `hkdf_salt` | Derived per Section 5.4.1 (`ctr` = `0x00`); value **[pinned-at-generation]** |
| Envelope metadata | The four required keys (the only conformant v1 form, Section 5.5.3) |

Expected geometry (derivable):

| Quantity | Expected value |
| --- | --- |
| `plaintext_size` `P` | 20480; `chunk_count` = 5 |
| Metadata plaintext | 50-byte map `{0: 1, 1: 20480, 2: "sha256", 3: <32 digest bytes>}` |
| `metadata_frame_len` `M` | 66 |
| Payload frame | bytes 194–20753 (5 chunks × 4112) |
| `footer_offset` | 20754; footer + 3806 zero-fill bytes |
| `stored_size_bytes` / blocks | 24576 / 6 |

Pinned outputs **[pinned-at-generation]**: the derived `hkdf_salt`
(Section 5.4.1); the exact 128 header bytes and `header_hash`; derived
`metadata_key` and `payload_key`; the exact metadata frame bytes; SHA-256 of
the payload frame; `stored_digest`. Because sealing is deterministic
(Section 5.4.1 property 2), the entire stored byte string is reproducible from
the inputs above with no test-only interfaces. Required equality: this
object's `plaintext_digest` MUST equal RAO-TV-P1's `stored_digest`, and the
value of metadata map key `3` (`plaintext_digest`, Section 5.5.3) MUST equal
that digest.

### 13.4. RAO-TV-D1 — Default Chunk Size

One vector MUST use `DEFAULT_CHUNK_SIZE`. Inputs: `chunk_size` 262144;
`object_id` `00000000-0000-4000-8000-000000000002`; `caller_object_id`
`rao-tv-d1`; `write_timestamp` `2026-01-01T00:00:00Z`;
`metadata_preservation` `minimal`; `manifest_file_id`
`00000000-0000-4000-8000-0000000000fe`; one file `v.bin`, `file_id`
`00000000-0000-4000-8000-000000000012`, contents 262145 bytes with byte `i` =
`i mod 256` (expected `file_sha256`
`c35991ad254f48ff8b02becb9f0cc56581e86a0b477b13e5ebb0030a3b91c848`,
`chunk_count` 2), sealed both plaintext and encrypted under the RAO-TV-E1 key
material (salt derived per Section 5.4.1). Pinned outputs as in 13.2/13.3
(digests only for the large streams; exact bytes for header, metadata frame,
and manifest).

### 13.5. Negative Vectors

Each contains exactly one fault and asserts the mapped error.

**Plaintext stream.** Writer-side (constructed via API): duplicate path;
duplicate `file_id`; manifest `file_id` colliding with a payload `file_id`;
reserved `_remanence/` path; control character in path; each non-canonical
path shape (`/abs`, `a/../b`, `./a`, `a//b`, `a/`); malformed `mtime`;
streamed payload with wrong hash; streamed payload with wrong size;
non-multiple-of-512 `chunk_size`; symlink/directory with nonzero size;
symlink missing target; directory path without trailing slash; a hardlink
whose target is absent or not a regular-file primary (`InvalidHardlinkTarget`).
Reader-side (byte vectors): wrong
`REMANENCE.format_id`; schema major 2; missing `REMANENCE.compression`;
`REMANENCE.compression=gzip`; `REMANENCE.encryption=aes-256-gcm`; declared
`REMANENCE.chunk_size` disagreeing with the supplied geometry; corrupted
header checksum; single zero EOF record; unknown typeflag; misaligned nonzero
payload; traversal-shaped effective path; an entry after the manifest; one
flipped payload bit (restore MUST fail `FileDigestMismatch`); truncated
payload; truncated pax body; pax record length out of bounds; pax record
missing `=`; pax record missing trailing newline; pax value with control
character; non-UTF-8 pax value. Manifest: non-canonical key order;
non-shortest integer encoding; indefinite-length item; float; tag; duplicate
map key; `schema_version` 2; `file_sha256` of wrong length; nesting depth
exceeding `MANIFEST_MAX_DEPTH`; manifest bytes disagreeing with the anchor;
manifest `chunk_size` disagreeing with the global header; unknown extra key
(MUST be accepted); two `file_entries` sharing a `path`; two `file_entries`
sharing a `file_id`. A restore-report vector reaches EOF without a manifest
and asserts the typed `MissingManifest` report rather than silent absence.

**Envelope.** Header: wrong magic; `header_len` ≠ 128; `format_version` 2;
unknown `suite_id`; `chunk_size` 0 and `chunk_size` not a multiple of 512;
nonzero `flags`; nonzero `reserved`; all-zero `key_id`; all-zero `hkdf_salt`;
all-NUL `object_id` field; interior-NUL `object_id`; non-UTF-8 `object_id`;
`metadata_frame_len` 16 and `metadata_frame_len` > 16 MiB. Cryptographic
binding: a flipped salt bit (structurally valid → `AeadAuthenticationFailed`);
`key_id` swapped to another known test key (`AeadAuthenticationFailed`); a
flipped ciphertext bit in chunk 1; chunks 1 and 2 transposed; wrong final flag
(a 6th chunk appended / final chunk re-sealed non-final); sealed metadata
deliberately misstating `plaintext_digest` (opens MUST fail
`PlaintextDigestMismatch`); an object sealed under an arbitrary (non-derived)
header salt, otherwise self-consistent (keyed open MUST fail
`SaltDerivationMismatch`). Metadata: each metadata-profile repertoire
violation (float, tag, indefinite length, duplicate key, non-shortest
encoding); missing key 1; `metadata_version` 2; `plaintext_size` not a
multiple of `chunk_size`; `plaintext_size` 0; overflow-implying
`plaintext_size`. Framing: EOF inside the metadata frame; EOF mid-chunk;
payload absent after metadata (`MissingFinalChunk`); footer bytes wrong at the
correct offset; one nonzero fill byte (`FillNotZero`); bytes appended past the
fill (`TrailingData` from keyed open/verify; the keyless classification of the
same input is advisory — Section 5.10). Inner cross-checks (defective-sealer
harness): inner `object_id` differing from the header; inner `chunk_size`
differing; inner `REMANENCE.encryption` ≠ `none` (`InnerObjectMismatch`).
Writer-side: sealing input with `P` not a multiple of `C`; `object_id` > 64
bytes; root key of 16 bytes (`InvalidRootKey`).

## 14. Conformance and Freeze Criteria

This specification is a draft until all of the following hold; after freeze,
no normative change is permitted other than errata that do not change the set
of valid objects (anything else is RAO version 2):

1. At least one complete implementation implements this document — both
   representations, the plaintext stream and the encryption envelope.
2. The Section 13 test vectors are published in the test-vector distribution
   that accompanies this specification, with every **[pinned-at-generation]**
   value generated, frozen, and passing, and the stated derivable values
   confirmed byte-exact. Each pinned cryptographic value is **independently
   re-derived** by a second implementation (different language/library) before
   freezing, so a reference-implementation bug cannot be frozen into the
   conformance anchor.
3. The plaintext-interop gates pass: GNU tar, bsdtar, and Python `tarfile`
   extract every positive plaintext vector byte-identically (Section 4.10),
   including RAO-TV-P1 and the plaintext RAO-TV-D1.
4. The `plaintext_digest` equality of Section 13.3 is demonstrated end to end:
   one build, two representations, shared logical identity, differing
   `stored_digest`.
5. PFR on ciphertext is tested by actually fetching mapped stored ranges,
   authenticating, decrypting, and comparing sliced plaintext to the source
   bytes (Section 6.3) — not by range arithmetic alone — including a range
   spanning a chunk boundary and a range in the final chunk.
6. Parity-over-ciphertext recovery is demonstrated: corrupt stored blocks of
   an encrypted object on a mock tape; the parity layer recovers keyless;
   keyed open then succeeds, and fails closed before recovery.
7. A failed seal (digest mismatch, size mismatch, injected I/O error) is
   proven to produce no footer and no durable catalog reference; a failed
   build likewise.
8. Coverage-guided fuzzing of the envelope header parser, both CBOR decoders,
   the tar record loop, and whole-object open/verify reaches a corpus plateau
   with no panics, crashes, hangs, or unbounded allocations.
9. A live round-trip passes on both virtualized and physical LTO tape
   hardware for both representations at two distinct `chunk_size` values,
   including standard-`tar -b` extraction of the plaintext copy.
10. A long-term-recovery drill: an independent party locates and verifies a
    payload file in a plaintext object using only this document, a generic
    CBOR tool, and standard `tar`; and opens an encrypted object using only
    this document, the key material, and generic crypto libraries — no
    reference source.
11. Salt-derivation conformance is demonstrated: resealing the same canonical
    object under one epoch reproduces a byte-identical encrypted copy; sealing
    objects differing only in content, and objects differing only in
    `object_id`, yields distinct salts, keys, and ciphertexts; and a keyed
    open rejects an otherwise self-consistent object whose header salt is not
    the derived value (Sections 5.4.1, 5.9 step 4).

## 15. IANA Considerations

This document has no IANA actions. The identifiers this specification
defines — the `rao-v1` stream format identifier, the `REMANENCE.` pax
keyword namespace, the `RAO1` envelope magic, and the `suite_id` value
`0x01` — are assigned by this document and governed by its versioning rules
(Section 10); no registry is established or required.

## 16. References

### 16.1. Normative References

- [RFC2119] — Bradner, S., "Key words for use in RFCs to Indicate
  Requirement Levels", BCP 14, RFC 2119, March 1997,
  <https://www.rfc-editor.org/info/rfc2119>.
- [RFC8174] — Leiba, B., "Ambiguity of Uppercase vs Lowercase in RFC 2119
  Key Words", BCP 14, RFC 8174, May 2017,
  <https://www.rfc-editor.org/info/rfc8174>.
- [RFC3339] — Klyne, G. and C. Newman, "Date and Time on the Internet:
  Timestamps", RFC 3339, July 2002,
  <https://www.rfc-editor.org/info/rfc3339>.
- [RFC3629] — Yergeau, F., "UTF-8, a transformation format of ISO 10646",
  STD 63, RFC 3629, November 2003,
  <https://www.rfc-editor.org/info/rfc3629>.
- [RFC5869] — Krawczyk, H. and P. Eronen, "HMAC-based Extract-and-Expand
  Key Derivation Function (HKDF)", RFC 5869, May 2010,
  <https://www.rfc-editor.org/info/rfc5869>.
- [RFC8439] — Nir, Y. and A. Langley, "ChaCha20 and Poly1305 for IETF
  Protocols", RFC 8439, June 2018,
  <https://www.rfc-editor.org/info/rfc8439>.
- [RFC8949] — Bormann, C. and P. Hoffman, "Concise Binary Object
  Representation (CBOR)", STD 94, RFC 8949, December 2020,
  <https://www.rfc-editor.org/info/rfc8949>.
- [FIPS180-4] — National Institute of Standards and Technology, "Secure
  Hash Standard (SHS)", FIPS PUB 180-4, August 2015 (defines SHA-256),
  <https://doi.org/10.6028/NIST.FIPS.180-4>.
- [POSIX-PAX] — IEEE Std 1003.1-2017 (POSIX.1-2017), Shell and Utilities
  volume, `pax` utility, "pax Interchange Format" (ustar and pax extended
  headers), <https://pubs.opengroup.org/onlinepubs/9699919799/>.
- [REMPARITY] — "Rem Tape Parity (REM-PARITY) Format, Version 1.0",
  companion specification published alongside this document: the parity
  layer of Sections 8.2 and 9. Normative only for implementations of the
  Section 8.2 tape binding.

### 16.2. Informative References

- [AGE] — Valsorda, F. and B. Cartwright-Cox, "The age file encryption
  format", C2SP (Community Cryptography Specification Project): the payload
  STREAM construction reference (informative provenance; Section 5.6 is
  self-contained), <https://c2sp.org/age>.
- [RFC2104] — Krawczyk, H., Bellare, M., and R. Canetti, "HMAC:
  Keyed-Hashing for Message Authentication", RFC 2104, February 1997,
  <https://www.rfc-editor.org/info/rfc2104>.
- [RFC8446] — Rescorla, E., "The Transport Layer Security (TLS) Protocol
  Version 1.3", RFC 8446, August 2018,
  <https://www.rfc-editor.org/info/rfc8446>.
- [PART-ORACLE] — Len, J., Grubbs, P., and T. Ristenpart, "Partitioning
  Oracle Attacks", 30th USENIX Security Symposium, 2021,
  <https://www.usenix.org/conference/usenixsecurity21/presentation/len>.
- [AEAD-COMMIT] — Albertini, A., Duong, T., Gueron, S., Kölbl, S.,
  Luykx, A., and S. Schmieg, "How to Abuse and Fix Authenticated Encryption
  Without Key Commitment", 31st USENIX Security Symposium, 2022,
  <https://www.usenix.org/conference/usenixsecurity22/presentation/albertini>.

---

## Appendix A. Worked Example (Informative)

This appendix derives the Section 13.2/13.3 expected values, exercising the
alignment equation, the manifest sizing, and the envelope geometry. It is
informative; the fixtures, once generated, are the conformance authority.

**RAO-TV-P1, `chunk_size` = 4096.** The global pax body's eight records
(Section 4.5.1 keywords with the TV-P1 values, `format_id` = `rao-v1`) measure
39 + 29 + 29 + 30 + 43 + 60 + 32 + 50 = 312 bytes, padding to 512; with its
`g` record the global header occupies bytes 0–1023.

*File 0* (`a/hello.txt`, 26 bytes): base pax records — `chunk_count` 27,
`compression` 30, `file_id` 58, `file_sha256` 90, `path` 20, `size` 11 — total
236 bytes. Pax record offset `O` = 1024; the alignment equation
`O + 512 + R + 512 ≡ 0 (mod 4096)` gives `R ≡ 2048`; minimum-pad payload
236 + 18 = 254 rounds to 512 (wrong residue), so the target is `R` = 2048 and
the pad record is `1812 REMANENCE.pad=` + 1792 spaces + LF (1812 = 4 digits +
1 space + 13 keyword + 1 `=` + 1792 + 1 LF; the fixed point holds). Pax
payload 236 + 1812 = 2048 exactly; the ustar header ends at
1024 + 512 + 2048 + 512 = 4096. Data at `BodyLba` 1; 26 bytes + record padding
ends the entry at 4608.

*File 1* (`b/pattern.bin`, 5000 bytes): base records 27 + 30 + 58 + 90 + 22 +
13 = 240. `O` = 4608 → `R ≡ 2560 (mod 4096)` → `R` = 2560; pad record
`2320 REMANENCE.pad=` + 2300 spaces + LF; data at 8192 (`BodyLba` 2),
`chunk_count` 2; entry ends at 13312 (5000 bytes pad to 5120).

*Manifest*: deterministic-CBOR sizes — top-level overhead 1; `object_id` 48;
`chunk_size` 14; `file_entries` 13 + 1 + (194 + 197); `schema_version` 16;
`object_metadata` 17; `caller_object_id` 26; `external_references` 21 — total
**548 bytes** (file-entry maps: 194 and 197 bytes respectively, with
`size_bytes` 26 encoding as `0x18 0x1a` and 5000 as `0x19 0x13 0x88`).
Manifest pax base records (with `executable=false`, `is_manifest=true`,
`path` 33, `size` 12) total 310; `O` = 13312 → `R ≡ 2048` → pad record
`1738 REMANENCE.pad=` + 1718 spaces + LF; manifest data at 16384 (`BodyLba` 4),
548 bytes padding to 17408. Tar EOF (1024 bytes) ends at 18432; zero fill to
**20480 = 5 blocks**.

**RAO-TV-E1.** `P` = 20480, `C` = 4096 → `chunk_count` 5. Metadata plaintext:
`a4 00 01 01 19 50 00 02 66 ... 03 58 20 ...` = 1 + 2 + 4 + 8 + 35 =
**50 bytes** → `M` = 66. Payload frame = 20480 + 80 = 20560 bytes at offset
194; `footer_offset` = 20754; stored pre-fill length 20770; fill 3806 →
**`stored_size_bytes` 24576 = 6 blocks**. The metadata zero nonce is
byte-identical to payload chunk 0's nonce here (chunk 0 is non-final, nonce
`00…00 00`) — exactly the collision that the metadata/payload key separation
renders harmless (Sections 5.5.2, 12.2).

## Appendix B. Design Rationale (Informative)

This appendix records the reasoning behind non-obvious decisions, so future
revisions do not silently reverse them.

### B.1. AEAD chunk size equals the body block

The AEAD plaintext chunk size is the object's `chunk_size` `C`, not a separate
constant. Because the canonical object's length is always an exact multiple of
`C` (the final zero fill guarantees it), plaintext chunk `i` coincides exactly
with inner body block `i`, so the per-file `BodyLba` index addresses
ciphertext with no second offset grid and no short-final-chunk cases. Tape
reads are block-granular anyway, so sub-block AEAD chunks would buy nothing on
the primary backend while multiplying per-chunk tag/nonce bookkeeping; tag
overhead is negligible (16/262144 ≈ 0.006%). Accepted cost: ciphertext chunks
are `C + 16` bytes and so do not land tape-block-aligned — one inner block's
ciphertext spans up to 3 stored blocks (Section 6.4) — and a byte-addressed
range fetch reads whole `C + 16` chunks. The alternative of `C − 16` plaintext
chunks (block-aligned ciphertext, misaligned plaintext) was rejected: it
destroys the 1:1 block↔chunk identity that keeps PFR arithmetic and parity
reasoning simple.

### B.2. Encryption is an envelope, never an in-stream flag

`REMANENCE.encryption` is permanently `none`; confidentiality is the
Section 5 envelope *around* the stream. Flagging encryption inside the global
header would break the shared `plaintext_digest` (the two copies' canonical
bytes would differ), break standard-`tar` extractability of the plaintext
copy, and place the marker inside the very bytes it claims are encrypted.

### B.3. Two identities: logical and physical

`plaintext_digest` (SHA-256 of the complete canonical object) is the logical
identity, shared by a plaintext and an encrypted copy of one object;
`stored_digest` (SHA-256 of one copy's stored bytes) is the physical identity
that backends scrub keyless. For a plaintext copy the two coincide. Copies
share the logical identity if and only if they wrap identical canonical
bytes ("build
once, fan out"); rebuilding from the same inputs with a new `object_id`,
timestamp, or `chunk_size` yields a new object, and per-file `file_sha256` is
the cross-rebuild invariant.

### B.4. Full final chunks only

`plaintext_size` is a positive exact multiple of `C`, every AEAD chunk is
exactly `C` bytes, and a canonical object is never empty (it always contains
at least the global header, manifest, and EOF). The age-style STREAM
construction is used unchanged; RAO objects simply occupy the subset of its
inputs where the final chunk is full. This removes short-final-chunk and
empty-payload special cases from the PFR and verification paths.

### B.5. The manifest is confidential in the encrypted representation

Encrypting the whole canonical stream, manifest included, means an encrypted
object leaks no filenames, sizes, count, or structure (Section 12.5).
Self-description holds *with the key*; keyless operation keeps exactly the
scrub/inventory surface of the plaintext header. A cleartext manifest beside
encrypted payloads was rejected: filenames, sizes, and counts are routinely
sensitive for the offsite/cloud copy, and the catalog plus the plaintext
working copy already provide keyless rebuild paths.

### B.6. Stored bytes are a block multiple; the fill is inside them

An encrypted object's stored bytes include zero fill from the footer to the
next `C` multiple, covered by `stored_digest` and parity. This gives
byte-stable fanout (one byte string, identical on tape, disk, and object
store, with one `stored_digest`), uniform block geometry for parity, no
backend-specific framing, and — because chunk counts are spaced `C + 16` apart
while the fill absorbs at most `C − 1` — a keyless-derivable geometry that
turns footer verification into an exact positional check (Section 5.10). Bytes
beyond `stored_size_bytes` are rejected (`TrailingData`).

### B.7. Two metadata layers, two CBOR profiles

The **manifest** (manifest-profile CBOR, text keys, inside the canonical
bytes) is the object's per-file index and is consequently encrypted in the
encrypted representation. The **envelope metadata frame** (metadata-profile
CBOR, integer keys) carries only what decryption itself needs
(`plaintext_size`, `plaintext_digest`). They are not merged: folding per-file
metadata into the envelope frame would move the index outside the
self-describing object and bloat a frame whose smallness bounds hostile input.
One deterministic-CBOR validator serves both profiles (Section 5.5.1).

### B.8. Empty AAD

Both AEADs use empty AAD because the bindings are already structural: the
object — including its `object_id`, `key_id`, salt, and geometry — is bound
through `header_hash` in the key derivation, and chunk index and finality are
bound through the nonce. Re-binding the same facts as AAD would add bytes and
a second mechanism without adding security.

### B.9. `stored_digest` is external

A digest over the complete stored bytes cannot live inside them, and a
truncated in-band variant would be a second, weaker integrity story. The
header field that *serves* keyless scrubbing is `object_id` — it lets a
scrubber look up the trusted external digest. Keyless scrub = external
`stored_digest` + (on tape) the parity layer's block CRCs.

### B.10. No unencrypted envelope

The plaintext representation is the bare canonical stream, not a stream inside
an unencrypted header/footer. Such a wrapper would break standard-`tar`
extractability — the plaintext copy's reason to exist — in exchange for
framing the stream already provides (self-description, digests).

### B.11. Derived salts, not random salts

`hkdf_salt` is derived from the root key, the object identifier, the content
digest, and the metadata bytes (Section 5.4.1), never drawn from a random
number generator. The catastrophic precondition — identical keys for different
plaintexts — then requires an identifier-reuse bug combined with a SHA-256
digest collision or a 2^−128 truncated-PRF collision (Section 12.1) rather
than an RNG failure; RNG failures are documented history (e.g. cloned VMs and
fork-without-reseed entropy reuse) while the hash and keyed-PRF assumptions
are ones the format already carries. Sealing becomes deterministic, so
resealing reproduces byte-identical encrypted copies and the test vectors are
reproducible by rule with no unsafe fixed-salt interface. The wire format and
the AEAD construction are untouched — the stored bytes are indistinguishable
from a random-salt object. A hedged variant mixing CSPRNG bytes into the
derivation was rejected (it re-imports a fixed-input test interface to solve a
non-problem here); a misuse-resistant AEAD such as AES-GCM-SIV was rejected (a
different construction for the same effective property).

### B.12. Envelope metadata is fixed in v1 and bound into the salt

If variable optional metadata existed, resealing the same object with
different metadata of the same encoded length could reproduce the same
`metadata_key` while the zero metadata nonce encrypted a *different* plaintext
— the nonce reuse [RFC8439] forbids. Two layers prevent this: version 1
defines no optional metadata keys (the frame is a deterministic function of
the canonical object), and SHA-256 of the metadata plaintext is bound into the
salt derivation and verified on every keyed open, so a future 1.x metadata
extension — or a defective sealer — cannot reach the reused-nonce state in a
readable object.

### B.13. Bootstrap manifest anchors are plaintext-copy-only

An encrypted object's on-tape bootstrap row carries no manifest anchors
(location, size, count, digest): manifest size correlates directly with member
count, and the bootstrap is plaintext on the same tape the envelope protects.
The integrity anchor for a decrypted manifest is the envelope's authenticated
`plaintext_digest`, which covers the manifest and every other canonical byte —
stronger than an external digest. What is forfeited is direct
LOCATE-to-manifest without a catalog; catalog-less recovery decrypts
sequentially, an acceptable cost over sequential media.

## Appendix C. Open Items Before Freeze (Informative)

1. **Pinned-at-generation vector values** (Section 13). The cryptographic
   outputs (derived salts and keys, header hashes, exact frame bytes, stored
   digests) must be generated, cross-checked against the derivable values of
   Appendix A, and frozen into the test-vector distribution. They are
   additionally re-derived by an independent second implementation before
   freezing (Section 14 criterion 2), so a single implementation's bug cannot
   become the conformance anchor.
2. **`object_id` ≤ 64 bytes in the envelope header** (Section 5.2). The
   plaintext stream allows an unbounded `object_id`; the envelope caps it at
   64 UTF-8 bytes (a frozen-field choice). Identifier-assigning systems must
   keep identifiers within 64 UTF-8 bytes if encrypted copies will be
   produced; UUID strings (36 bytes) leave ample headroom. Confirming that
   no deployed assigner mints longer identifiers is a freeze item.
3. **Tape block size = `chunk_size`** (Section 8.2). A copy must be written to
   a pool whose configured tape block size equals the object's `chunk_size`;
   in practice one fleet-wide `chunk_size` (the 256 KiB default) makes this a
   non-issue. Confirming this against each deployment's tape block-size
   configuration is a freeze item.
4. **Verifier/salvage profile.** At least one implementation must expose the
   Section 7.4 Verifier profile and the Section 4.9 salvage mode as explicit
   operator-selected capabilities: manifest/archive correspondence, final-fill
   zero checking, and report-all-nonconformities behavior must be exercised
   independently from the default restore path.
5. **Restore path hardening.** Filesystem restore must use component-relative
   symlink-resistant discipline (`openat`/`O_NOFOLLOW` or equivalent) for
   directories, regular files, and symlink materialization before symlink
   vectors are frozen as operational restore evidence.
6. **Catalog salt-audit check.** The Section 12.1 defense-in-depth
   duplicate-salt check needs `hkdf_salt` persisted in encrypted-copy catalog
   records alongside `key_id`, `plaintext_digest`, and `stored_digest`;
   adding that field is a freeze item for any deployment whose catalog is to
   enforce the check directly.

## Author's Address

The ArchiveTech Project
Website: https://archivetech.org
Email: specs@archivetech.org
Reference implementation: https://github.com/archivetechie/remanence
