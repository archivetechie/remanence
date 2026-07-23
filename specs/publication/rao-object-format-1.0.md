# RAO (Rem Archive Object) Format Specification, Version 1.0

| | |
| --- | --- |
| Status | Publication specification |
| Version | 1.0 |
| Date | 2026-07-22 (v1.2.0 freeze increment) |
| Concept DOI (all versions) | [10.5281/zenodo.21425126](https://doi.org/10.5281/zenodo.21425126) |
| Version DOI (this release) | assigned at release (the concept DOI above always resolves to the latest version) |
| Envelope magic | `RAO1` (encrypted representation) |
| Stream format identifier | `rao-v1` (plaintext representation) |
| On-tape format version | `2` (HPKE wrapped-DEK envelope) |
| Default file extension | `.rao` |

## Status of This Document

This document is the publication specification for the RAO format. It is the
normative fixed point for the format it defines: an implementation is
validated against this document, not the reverse. It consolidates the base
container, extended-attribute preservation, and HPKE envelope encryption into
one independently implementable baseline.

RAO's tape binding depends normatively on the REM-PARITY specification
([REMPARITY]), which is at `Draft for review` and not yet frozen. That
dependency is therefore **provisional and version-pinned**: the tape-binding
clauses of this document (the parity-layer references in Sections 4.9, 8.2, 9,
12.5) are stable against the specific REM-PARITY revision cited in the
References, and MAY change when REM-PARITY freezes. The file, object-store, and
encrypted-envelope portions of this document do not depend on REM-PARITY and are
not provisional.

## Abstract

This document specifies the Rem Archive Object (RAO) format, specification
version 1.0: a backend-independent byte format for large archival objects. An RAO object
bundles many named file payloads into one self-describing unit — a constrained
POSIX pax tar stream carrying per-file SHA-256 identities, closed-form
byte-range addressing, and a deterministic CBOR manifest — and exists in
exactly two representations: **plaintext**, the bare container stream,
extractable by any standard `tar`; and **encrypted**, the same byte stream
sealed inside a confidential authenticated envelope using an HPKE-wrapped
per-object data-encryption key, HKDF-SHA-256 key derivation, and a chunked
ChaCha20-Poly1305 stream construction. Both
representations of one object share a logical identity (`plaintext_digest`);
each stored copy has a physical identity (`stored_digest`) that backends scrub
without keys. Encryption preserves partial file restore:
authenticated-encryption (AEAD) chunks coincide
one-to-one with the object's body blocks, so a per-file block index addresses
ciphertext by closed-form arithmetic. The format is designed for single-pass
writing, byte-stable fanout to tape, disk, and object storage, parity
protection over stored bytes, and long-term recovery from this document and
its static test vectors alone. Canonical plaintext construction is
deterministic; every encrypted seal uses fresh randomness and therefore
normally produces different envelope bytes.

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
14. [Conformance](#14-conformance)
15. [IANA Considerations](#15-iana-considerations)
16. [References](#16-references)

Appendix A. [Worked Example (Informative)](#appendix-a-worked-example-informative)
Appendix B. [Design Rationale (Informative)](#appendix-b-design-rationale-informative)
Appendix C. [Revision History (Informative)](#appendix-c-revision-history-informative)

---

## 1. Introduction

### 1.1. Purpose and Design Goals

RAO wraps a set of named file payloads into one archival object. The format
originates in **Remanence**, an open archival tape stack that serves as this
specification's reference implementation [REMANENCE]. This document
specifies the format completely, so that it stands alone from any
implementation; the name survives in the format itself only as fixed wire
identifiers — the `REMANENCE.` vendor-keyword namespace and the `_remanence/`
manifest path (Section 4). RAO's design goals, in priority order:

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
6. **Deterministic canonical plaintext representation.** Given identical
   inputs and options, every conformant Builder produces the identical
   plaintext stream. Encrypted envelopes intentionally vary because every
   seal uses a fresh DEK and fresh HPKE encapsulation randomness.
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

### 1.3. Relationship to Adjacent Components

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
- **Key custody is external** (Section 5.3). An encrypted object carries
  recipient epoch identifiers and HPKE-wrapped copies of its DEK, never a
  plaintext key.

### 1.4. Non-Goals

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
(Section 4.3.1); selected POSIX extended attributes are preserved as specified
in Section 4.7.3.
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
  encrypted representation; Section 5.9).
- **Planner**: computes a plaintext object's exact layout and block count
  without payload bytes (Section 4.9). Planning determinism extends to the
  envelope: the encrypted stored size is a closed form of the plaintext size
  (Section 5.7).
- **Reader**: recovers entries from an object in either representation
  (Sections 4.9, 5.10).
- **Repacker**: re-emits an object or manifest while preserving its entries (a
  re-pack that does not re-capture from a source filesystem). Its preservation
  obligations are stated in Sections 4.7.5 and 10.
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
  key frame, metadata frame, payload frame, footer, and final fill (Section 5.1).
- **Data-encryption key (DEK)**: the fresh 32-byte per-object secret used as
  the root of an encrypted envelope's key schedule.
- **Recipient epoch**: one X25519 key pair identified by a 16-byte
  `recipient_epoch_id` and a printable recovery label.
- **Deterministic CBOR**: the canonical CBOR encoding rules of Section 4.7.1,
  used in two profiles — the **manifest profile** (text keys; Section 4.7)
  and the **metadata profile** (unsigned-integer keys; Section 5.6).

### 2.4. Integer, Byte, and Text Conventions

All fixed-width integers in the envelope header are unsigned and encoded
big-endian (network byte order). Byte offsets are zero-based. `KiB` = 2^10
bytes; `MiB` = 2^20 bytes. Hexadecimal values are prefixed `0x`. ustar numeric
fields are ASCII octal (Section 4.3). All other text in the format — pax
keywords and values, paths, manifest and metadata text strings — is UTF-8
[RFC3629]; pax keywords are additionally restricted to ASCII. The functions
`roundup(x, C)` and `roundup512(x)` denote the smallest multiple of `C`
(respectively 512) that is greater than or equal to `x`; when `x` is already
a multiple the result is `x` itself. Equivalently,
`roundup(x, C) = x + ((C − (x mod C)) mod C)`. SHA-256 is the
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
| `STREAM_SCHEMA_VERSION` | `1.0` without preserved xattrs; `1.1` with any preserved xattrs | Value of the global `REMANENCE.schema_version` keyword |
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
| `RAO_FORMAT_VERSION_HPKE` | 2 | HPKE envelope `format_version` |
| `RAO_SUITE_HKDF_CHACHA` | `0x01` | `suite_id`: HKDF-SHA-256 + ChaCha20-Poly1305 |
| `RAO_WRAP_SUITE_HPKE_V1` | `0x01` | HPKE Base wrap suite |
| `RAO_KEY_FRAME_MAX_LEN` | 4096 | Maximum key-frame length |
| `RAO_KEY_FRAME_MAX_SLOTS` | 8 | Maximum recipient slots |
| `RAO_SALT_LEN` | 16 | `hkdf_salt` length in bytes |
| `RAO_OBJECT_ID_FIELD_LEN` | 64 | Fixed `object_id` header field length in bytes |
| `RAO_TAG_LEN` | 16 | Poly1305 tag length in bytes |
| `RAO_NONCE_LEN` | 12 | ChaCha20-Poly1305 nonce length in bytes |
| `RAO_KEY_LEN` | 32 | Derived AEAD key length in bytes |
| `RAO_MAX_METADATA_FRAME_LEN` | 16777216 (16 MiB) | Maximum envelope metadata frame length |
| `RAO_MAX_CBOR_NESTING_DEPTH` | 32 | Maximum envelope metadata nesting depth |
| `RAO_MAX_METADATA_ITEMS` | 65536 | Maximum envelope metadata data-item count |
| `RAO_FOOTER` | `RAO1_STREAM_END.` | 16-byte completion footer (Section 5.8); hex `52 41 4F 31 5F 53 54 52 45 41 4D 5F 45 4E 44 2E` |
| `LABEL_SALT` | `rao2-salt-v1` | Salt-derivation info label, 12 ASCII bytes |
| `LABEL_OBJECT` | `rao2-object-v1` | Object-secret info label, 14 ASCII bytes |
| `LABEL_METADATA` | `rao2-metadata-v1` | Metadata-key info label, 16 ASCII bytes |
| `LABEL_PAYLOAD` | `rao2-payload-v1` | Payload-key info label, 15 ASCII bytes |
| `WRAP_INFO_PREFIX` | `rao-wrap-v1` followed by NUL | 12-byte HPKE info prefix |

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
| `encrypted` | The Section 5 envelope: plaintext header ‖ key frame ‖ encrypted metadata frame ‖ chunked AEAD ciphertext of the canonical plaintext object ‖ footer ‖ zero fill | Confidential and authenticated; self-describing **with the key**; opaque without it |

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
| `plaintext_digest` | The **complete canonical plaintext object** bytes | Encrypted copies: inside the authenticated metadata frame (Section 5.6). All copies: catalog | No (for encrypted copies) |
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

Informative: the four digests map to preservation fixity roles —
`file_sha256` is per-file content fixity (PREMIS bitstream fixity [PREMIS]),
`manifest_sha256` is structural-index fixity, `plaintext_digest` is
whole-object (content-plus-structure) fixity (OAIS Fixity Information over the
Content plus Packaging [OAIS]), and `stored_digest` is per-copy fixity. Because
`plaintext_digest` covers the manifest, any change to how the object is
interpreted — a path, an entry type, a link target, chunk geometry, or a
per-file `file_sha256` — changes `plaintext_digest`. This document commits to
SHA-256; algorithm agility is out of scope and would be a
successor-specification concern.

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
FIFO, socket, and other special entries (Section 1.4); accepting an
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
| `REMANENCE.object_id` | Object identifier of 1–64 non-NUL UTF-8 bytes (a UUID string in practice; opaque to this format). The 1–64-byte bound is uniform across representations — it matches the encrypted envelope field (Section 5.2) and lets the REM-PARITY tape binding ([REMPARITY] bootstrap key 4) carry the identifier verbatim. |
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
hardlink entry. Primary selection MUST be deterministic and is defined over the entries
the object emits: the primary is the first, in archive order, of the set's
names that the object emits as entries. If the object emits only one name of
the set, that name is a plain regular entry (no hardlink entry).

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
| `object_metadata` | map | Empty (`{}`), or the inventory of Section 4.7.6, optionally with an `ext` container (Section 4.7.5) |
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
| `metadata_preservation_data` | map | Empty, the xattr container of Section 4.7.3, an `ext` container (Section 4.7.5), or both; hardlink entries MUST use an empty map |

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
3. A Consumer MUST treat unknown bare keys (top-level or per-entry, including
   unknown bare keys within `metadata_preservation_data` and
   `object_metadata`) as **reserved for future revisions of this document** —
   ignore them, and do not use them for third-party data (which lives only
   under `ext`, Section 4.7.5). It MUST NOT reject a manifest merely because a
   reserved map or array is non-empty.
4. When both the manifest and the archive entries are available, a Consumer
   SHOULD verify they correspond exactly — same paths, entry types, link
   targets, sizes, hashes where present, and chunk geometry, with no extras on either side (Verifiers MUST;
   Section 7.4).

#### 4.7.3. Extended-Attribute Preservation

An entry's `metadata_preservation_data` MAY contain the following map entry:

```text
"xattrs" : { <name> : <value>, ... }
```

`<name>` is a nonempty CBOR text string containing the attribute name. It MUST
be valid UTF-8 [RFC3629] and MUST NOT contain an ASCII control byte below
`0x20`; no escaping is defined. A Writer MUST reject a name violating these
rules.
The **namespace** of an attribute name is the substring preceding its first
`.`; a name containing no `.` has no namespace. This document's validity rule
for names is unchanged (nonempty UTF-8, no ASCII control byte below `0x20`);
the namespace derivation is a classification rule, not an acceptance rule, and
does not shrink the set of valid names.

The name is stored in **canonical wire form** as `namespace.name`. This is a
Writer obligation: a Writer on a platform whose native attribute model differs
(a separate namespace argument; a flat namespace; case-folding storage) MUST
map its native namespace to the canonical prefix (for example, a `user.`
namespace attribute is `user.name`) deterministically and MUST NOT remap one
namespace onto another. A native attribute a Writer cannot represent with a
derivable namespace — including a name with no `.`, or one a case-folding
store cannot round-trip without altering case — is not captured, and its
omission is reported as ingest policy (Section 4.7 is silent on ingest
selection; Section 12.10 governs restore, not capture). The canonical name
bytes are identical across independent Writers for the same native attribute
on the same platform; whole-manifest byte identity additionally requires
identical object parameters (Section 1.1 goal 6).

`<value>` is a CBOR byte string containing the raw attribute value without a
textual encoding. The `xattrs` map follows the deterministic encoding rules of
Section 4.7.1, including encoded-key ordering and the prohibition on duplicate
names. Readers MUST ignore unknown keys in `metadata_preservation_data`
(reserved for future revisions; third-party data lives under `ext`,
Section 4.7.5).

An entry with no preserved xattrs MUST carry an empty
`metadata_preservation_data` map. A hardlink entry MUST carry an empty map;
the shared file's restored xattrs come from the regular-file primary named by
`link_target`. Ownership, ACLs as a separate RAO semantic, and mode bits beyond
`executable` remain outside this format. `mtime` is already represented by the
pax `mtime` keyword.

A Writer that emits no preserved xattrs anywhere MUST set
`REMANENCE.schema_version = 1.0`. A Writer that emits at least one preserved
xattr MUST set it to `1.1`. In both cases the manifest CBOR `schema_version`
integer remains 1. This gate does not depend on encrypted-envelope
`format_version`; the stream and envelope version axes are independent
(Section 10).

Which xattrs an ingesting system selects is policy outside this byte format.
This document defines a **portable core** and an **extension tier**,
distinguished by what a Restoring Consumer applies by default, not by what is
carried — both tiers are carried faithfully. The portable core is the `user.`
attribute namespace, which a Restoring Consumer is permitted to apply by
default (Section 12.10). Every attribute not in the `user.` namespace and every
extension (Section 4.7.5) is the extension tier: carried, but on restore
**carry-only** — applied only when explicit operator policy names it
(Section 12.10). No registered disposition or external list can cause an
extension-tier item to be applied by default in version 1.0.

A Reader implementing xattr preservation MUST surface them to its caller. A
Restoring Consumer MAY reapply attributes, subject to Section 12.10, and MUST
surface any application failure rather than silently declaring success.

#### 4.7.4. Within-Stream Chain of Trust

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

#### 4.7.5. Extension Containers

An entry's `metadata_preservation_data` map and the object-level
`object_metadata` map (Section 4.7.2) MAY carry a single reserved indirection
key, `ext`, whose value is a map; a hardlink entry's
`metadata_preservation_data` MUST remain empty (Section 4.7.3) and MUST NOT
carry `ext`. A non-map `ext` value makes the manifest nonconformant
(`ManifestInvalid`). Every bare (non-`ext`) key in these two maps is reserved
to this specification and its successors; third-party and platform-specific
data MUST live only under `ext`. (Section 4.7.2 obligation 3 is amended
accordingly: unknown bare keys are reserved-for-future-use — ignored, not an
extension point.)

Each member of an `ext` map is one extension, keyed by an **extension name**:
either a **reverse-DNS name** — lowercase, containing at least one `.`, in a
domain the author controls (for example `org.example.thing`) — requiring no
registration; or a **registered short name** — lowercase, hyphen-separated,
containing no `.` — from the community list (Section 15). The presence of a
`.` distinguishes the two. A malformed or uppercase extension name is treated
as unrecognized: it is ignored and carry-only, not a reason to reject the
object. An `ext` member value MUST use the manifest CBOR profile of
Section 4.7.1 (definite-length items, the permitted major types only) and
counts against the Section 4.7.1 depth limit; a non-conforming `ext` value
makes the whole manifest nonconformant (`Cbor`).

Extension processing is fail-safe, carry-only, and additive:

- A Consumer **recognizes** an extension only if it implements that
  extension's semantics; knowing an extension's name or any registered
  disposition is not recognition.
- A Consumer MUST ignore an `ext` member it does not recognize and MUST NOT
  reject an object for its presence.
- A Restoring Consumer MUST NOT apply any extension to system state unless
  explicit operator policy names it (Section 12.10); in version 1.0 no
  extension is applied by default. An unrecognized extension is always
  carry-only.
- A Repacker (Section 2.2) MUST reproduce the canonical CBOR encoding of every
  `ext` member it does not recognize unchanged (equivalently: it preserves the
  decoded value; under Section 4.7.1 the canonical re-encoding is identical).
  Silently dropping an unrecognized extension is nonconformant.
- Extensions are **ancillary by definition**: an extension MUST NOT be
  required to interpret an object's content or structure correctly. A feature
  a conformant Consumer must understand to read an object is a new stream
  `format_id` (Section 10), never an extension.

`ext` keys participate in the Section 4.7.1 deterministic ordering; their
presence changes `manifest_sha256` and `plaintext_digest` as any manifest
content does. `ext` and `object_metadata` presence do NOT affect
`REMANENCE.schema_version` (Section 4.7.3): the 1.0/1.1 gate remains keyed
solely to preserved xattrs.

#### 4.7.6. Object Metadata Inventory

When any entry, or the object itself, carries an attribute outside the `user.`
namespace or any `ext` member, the object's `object_metadata` map MUST carry an
inventory so a holder can determine what non-core metadata the object contains
without decoding `file_entries`. The inventory is a map with exactly these
keys:

| Key | Type | Value |
| --- | --- | --- |
| `attribute_namespaces` | array of text | the set of distinct non-`user.` attribute namespaces (Section 4.7.3) present across all entries, sorted in the order Section 4.7.1 defines for text map keys (encoded length prefix, then key bytes) |
| `extensions` | array of text | the set of distinct `ext` extension names present across all entries and in `object_metadata`, sorted in the order Section 4.7.1 defines for text map keys (encoded length prefix, then key bytes) |

Both arrays carry names only; attribute values and per-entry detail MUST NOT
appear. An empty array is omitted (its key absent). For verification, an
absent inventory key is treated as an empty array; a present empty array is
accepted (writer determinism is not a read-acceptance rule, Section 4.4.2). An
object carrying only the portable core and no `ext` MUST leave
`object_metadata` empty (`{}`). A Consumer MUST treat an unrecognized
`object_metadata` key as reserved-for-future-use and ignore it (Section 4.7.2
obligation 3).

**Verifier obligation:** a Verifier (Section 7.4) MUST confirm the inventory
is exact — `attribute_namespaces` equals the set of non-`user.` namespaces
actually present, and `extensions` equals the set of `ext` names actually
present across all entries and in `object_metadata` — and MUST reject a
mismatch (`ManifestInvalid`). A holder MAY rely on the inventory as a
disclosure-screening surface only for an object that has passed Verifier
validation.

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

A Writer that re-captures an object from a previously restored tree MUST carry
forward, unchanged, every `ext` member present in the source object's manifest
that it does not recognize.

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
   Partial-range reads cannot verify a whole-file `file_sha256`. Their integrity
   depends on representation and backend, and a range-read implementation MUST
   report which of the three it provides rather than imply hash-verified content:
   (a) a **parity-protected tape** plaintext copy is covered by the parity
   layer's per-block CRCs ([REMPARITY]) — damage detection, not adversarial
   authentication (CRC-64 confirms a guessed block); (b) an **encrypted** copy on
   any backend authenticates every byte range through the per-chunk AEAD tag
   (Section 6.3): a chunk whose tag fails aborts the range read (Section 5.10),
   so range reads of the encrypted representation are cryptographically verified
   end to end; (c) a **plaintext copy on a byte-addressed backend without the
   parity layer** (a file or object store) has **no per-range integrity by
   construction** — a verifying range read there requires either the encrypted
   representation or a whole-file `file_sha256`/`plaintext_digest` check, which
   reads the whole file. Implementations MUST NOT present case (c) as
   integrity-verified.
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
claim (Section 12.10). With all RAO-specific metadata lost, a Scanner can still
walk the archive using only tar rules — header, `size`, `roundup512(size)`,
repeat — recovering payload bytes, hardlink relationships, symlink targets,
directory entries, and names; it loses only chunk addressing (irrelevant when scanning) and
verification (recoverable from the manifest if its blocks survive).
Conformance requires demonstrated extraction equality by GNU tar, bsdtar, and
Python `tarfile` (Section 14).

## 5. Encrypted Representation

### 5.1. Frame Sequence and Version Gate

An encrypted RAO object has one layout:

```text
scalar header (128) || key frame (K) || metadata frame (M) ||
payload chunks || footer (16) || zero fill
```

The header, key frame, footer, and fill are plaintext. The metadata frame
and payload chunks are ChaCha20-Poly1305 ciphertext. The payload plaintext is
the complete canonical object of Section 4, manifest included. Total stored
length MUST be a positive multiple of `chunk_size`.

`format_version` is an on-tape field, not this document's version. A Reader
MUST accept only value 2 and requires a matching recipient private key. It
MUST NOT attempt another key mode after any parse, key-resolution, unwrap, or
authentication failure. Value 1 is permanently reserved as stated in
Section 10.

### 5.2. Scalar Header

The scalar header is exactly 128 bytes. All integers are unsigned big-endian.

| Offset | Length | Name | Type | Required value or meaning |
| --- | ---: | --- | --- | --- |
| `0x00` | 4 | `magic` | ASCII | `RAO1` |
| `0x04` | 2 | `header_len` | `uint16` | 128 |
| `0x06` | 1 | `format_version` | `uint8` | 2 |
| `0x07` | 1 | `suite_id` | `uint8` | `0x01` (HKDF-SHA-256 and ChaCha20-Poly1305) |
| `0x08` | 4 | `chunk_size` | `uint32` | Positive multiple of 512; equal to the inner stream value |
| `0x0C` | 4 | `flags` | `uint32` | Zero |
| `0x10` | 16 | `reserved` | bytes | Zero |
| `0x20` | 16 | `hkdf_salt` | bytes | Nonzero salt derived by Section 5.5 |
| `0x30` | 8 | `metadata_frame_len` | `uint64` | `M`, including the 16-byte tag; 17 through 16777216 |
| `0x38` | 1 | `wrap_suite` | `uint8` | `0x01` |
| `0x39` | 3 | `reserved` | bytes | Zero |
| `0x3C` | 4 | `key_frame_len` | `uint32` | Canonical key-frame length `K` |
| `0x40` | 64 | `object_id` | UTF-8 field | 1–64 non-NUL bytes, then NUL padding |

Bytes `0x10..0x20` and `0x39..0x3C` are reserved and MUST be zero.
`wrap_suite = 0x01`, and `key_frame_len` MUST be between 103 and 4096
inclusive. Although `103` is the
smallest syntactically possible one-slot frame, a conforming Sealer emits
at least two slots, so its emitted minimum is 201 bytes when both labels are
empty. A zero wrap suite or absent/undersized key frame is not a valid object
in this specification.

The `object_id` value is the inner `REMANENCE.object_id`. It contains no NUL,
is valid UTF-8, and is right-padded with zero bytes. Readers MUST reject an
all-zero field, an interior NUL followed by a nonzero byte, invalid UTF-8, or
a value longer than 64 bytes. The header's `chunk_size` and `object_id` MUST
equal the authenticated inner values after opening.

The byte-exact beginning of an envelope header with `chunk_size = 4096`,
`metadata_frame_len = 64`, `key_frame_len = 103`, object id `object-2`, and
the illustrative salt `02` repeated 16 times is:

```text
52 41 4f 31 00 80 02 01 00 00 10 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
02 02 02 02 02 02 02 02 02 02 02 02 02 02 02 02
00 00 00 00 00 00 00 40 01 00 00 00 00 00 00 67
6f 62 6a 65 63 74 2d 32 00 ... 00
```

The final line occupies the 64-byte `object_id` field. The repeated salt is a
layout illustration, not a derived conformance value.

### 5.3. The Key Frame and HPKE Wrapping

The key frame begins immediately at byte 128 and occupies exactly
`key_frame_len` bytes. Its canonical grammar is:

| Relative offset | Length | Field | Constraint |
| --- | ---: | --- | --- |
| `0` | 4 | magic | ASCII `RAOK` (`52 41 4f 4b`) |
| `4` | 1 | `slot_count` | 1 through 8 |
| repeated | 1 | `slot_index` | Strictly increasing across slots |
| repeated | 16 | `recipient_epoch_id` | Opaque epoch identifier |
| repeated | 1 | `label_len` | 0 through 32 |
| repeated | `label_len` | `epoch_label` | Bytes `0x20` through `0x7E` only |
| repeated | 32 | `enc` | X25519 HPKE encapsulated key |
| repeated | 48 | `ciphertext` | Wrapped 32-byte DEK plus 16-byte tag |

Every integer is big-endian; the one-byte integers have no byte-order
ambiguity. A key frame has length

```text
K = 5 + sum_over_slots(98 + label_len)
```

and MUST consume exactly `K` bytes. A Reader MUST reject truncation, trailing
bytes, a non-increasing or duplicate slot index, a duplicate
`recipient_epoch_id`, an invalid label, an invalid slot count, or a frame
outside the header's length bounds. A Sealer MUST emit at least two slots with
distinct `recipient_epoch_id` values and MUST fail the entire seal if any
recipient cannot be wrapped. A Reader accepts any structurally valid frame
with at least one slot. Slot order is the strictly increasing `slot_index`
order; callers therefore MUST supply values that serialize in that order.

Wrap suite `0x01` is HPKE Base mode [RFC9180] with
DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, and ChaCha20-Poly1305. The HPKE
plaintext is the 32-byte DEK; HPKE AAD is empty. Each slot uses fresh
encapsulation randomness. The exact 95-byte HPKE `info` value is:

```text
"rao-wrap-v1\0"                         12 bytes
|| object_id_field                      64 bytes
|| recipient_epoch_id                  16 bytes
|| slot_index                            1 byte
|| 0x02                                  1 byte  (format_version)
|| 0x01                                  1 byte  (wrap_suite)
```

For `object_id = "obj"`, epoch id `44` repeated 16 times, and slot 7, it is:

```text
72 61 6f 2d 77 72 61 70 2d 76 31 00
6f 62 6a 00 ... 00
44 44 44 44 44 44 44 44 44 44 44 44 44 44 44 44 07 02 01
```

The middle field is exactly 64 bytes. A wrapped DEK is thereby bound to the
object id, recipient epoch, slot, format version, and wrap suite. A Reader
selects the slot whose epoch id equals the supplied private key's epoch id;
absence is a hard recipient-epoch mismatch.

### 5.4. Key Inputs and Identification

The Sealer generates a fresh uniformly random 32-byte DEK for
every object and wraps it independently to every recipient slot. The Sealer
MUST obtain both DEK and encapsulation randomness from a fallible,
operating-system-backed CSPRNG and MUST fail closed if entropy is unavailable.
Recipient public keys and fingerprints are custody inputs outside this byte
format. A private key for any epoch named in the frame has unilateral ability
to decrypt every object wrapped to that epoch; operational threshold ceremonies
do not change that cryptographic fact.

### 5.5. Salt and Object-Key Derivation

`HKDF(ikm, salt, info, len)` means HKDF-Extract followed by HKDF-Expand as in
[RFC5869]. `SHA-256` is [FIPS180-4]. Let `metadata_hash` be SHA-256 of the
canonical metadata plaintext and `object_id_field` the exact 64 header bytes.
For the first `ctr` in `0x00..=0xFF` whose output is nonzero, derive:

```text
hkdf_salt = HKDF(DEK, empty,
  "rao2-salt-v1" || ctr || object_id_field || plaintext_digest || metadata_hash,
  16)
```

The one-byte `ctr` follows the ASCII label with no separator. A Sealer MUST
derive the salt and MUST NOT accept a caller-supplied value. An opener MUST
rederive it after metadata authentication and reject a mismatch.

Define `header_hash = SHA-256(exact 128-byte scalar header || exact key
frame)`.

Then derive three distinct 32-byte keys:

```text
object_secret = HKDF(DEK, hkdf_salt,
                     "rao2-object-v1" || header_hash, 32)
metadata_key  = HKDF(object_secret, empty, "rao2-metadata-v1", 32)
payload_key   = HKDF(object_secret, empty, "rao2-payload-v1", 32)
```

This binds every scalar-header byte and every key-frame byte through the
derived AEAD keys. Rewrapping a key frame without resealing metadata
and payload is therefore impossible and MUST NOT be attempted.

### 5.6. Metadata Frame

The metadata plaintext is one deterministic-CBOR [RFC8949] map using
unsigned-integer keys. The writer form has exactly four entries:

| Key | Value |
| ---: | --- |
| 0 | unsigned integer `1` (`metadata_version`) |
| 1 | positive `plaintext_size`, an exact multiple of `chunk_size` |
| 2 | text `sha256` |
| 3 | 32-byte `plaintext_digest` |

The keys are encoded in ascending deterministic order, using shortest-form
integer and length encodings. A Reader MAY skip unknown keys but MUST enforce
canonical map ordering, unique keys, valid UTF-8, a maximum nesting depth of
32, and at most 65536 decoded items. It MUST reject missing or invalid required
fields and trailing bytes. The accepted CBOR repertoire is unsigned integers,
definite byte/text strings, definite arrays/maps, and simple values false,
true, and null; negative integers, tags, floats, indefinite forms, and other
simple values are invalid.

The frame is `ChaCha20-Poly1305(metadata_key, nonce = 12 zero bytes,
AAD = empty, plaintext = metadata CBOR)`, stored as the ciphertext immediately
followed by the 16-byte Poly1305 tag. Its stored length is plaintext length
plus the 16-byte tag and MUST equal `metadata_frame_len`.

### 5.7. Payload Frame

Let `P = plaintext_size` and `C = chunk_size`. Both are authenticated through
the metadata and header-bound key schedule. `P` is positive and exactly
divisible by `C`; therefore `N = P / C` full plaintext chunks are emitted.
Chunk `i`, for `0 <= i < N`, is encrypted with `payload_key`, empty AAD, and
this 12-byte nonce:

```text
00 00 00 || uint64_be(i) || final_flag
```

`final_flag` is `0x01` exactly when `i = N - 1`, otherwise `0x00`. Each stored
chunk is exactly `C + 16` bytes: the `C` ciphertext bytes immediately followed
by the 16-byte Poly1305 tag. The payload-frame length is
`P + 16 * N`. Readers MUST compute finality; they MUST NOT infer it by probing
or accept a short final chunk.

### 5.8. Footer, Fill, and Geometry

The completion footer is the literal 16 bytes:

```text
ASCII: RAO1_STREAM_END.
hex:   52 41 4f 31 5f 53 54 52 45 41 4d 5f 45 4e 44 2e
```

It follows the last payload chunk. Zero bytes then fill through the next
`C`-byte boundary. The fill is part of the stored bytes and `stored_digest`.
Let `K` be the header's key-frame length:

```text
payload_len       = P + 16 * (P / C)
footer_offset     = 128 + K + M + payload_len
stored_size       = roundup(footer_offset + 16, C)
cipher_offset(i)  = 128 + K + M + i * (C + 16)
```

Given only stored size and the public header, a Keyless Verifier computes:

```text
N = floor((stored_size - 128 - K - M - 16) / (C + 16))
footer_offset = 128 + K + M + N * (C + 16)
```

It MUST additionally require `N > 0`, `stored_size mod C = 0`,
`roundup(footer_offset + 16, C) = stored_size`, the footer at that exact
offset, and all remaining bytes zero. These checks validate geometry and
completion but do not authenticate the object.

### 5.9. Sealing

A Sealer MUST perform the following logical sequence, using checked
arithmetic throughout:

1. Validate `C`, `P`, `object_id`, expected digest, and recipient set.
2. Construct the canonical metadata plaintext.
3. Generate the DEK, derive the salt, wrap the DEK to every configured
   recipient, and serialize the canonical key frame.
4. Serialize the final scalar header, including `M`, salt, wrap suite, and
   `K`.
5. Hash the scalar header plus key frame; derive the object, metadata, and
   payload keys.
6. Emit header, key frame, encrypted metadata, and every encrypted
   full payload chunk while recomputing plaintext size and SHA-256.
7. Reject any expected/observed size or digest mismatch and reject source
   bytes beyond `P`.
8. Emit the footer and zero fill. A failed seal MUST NOT be represented as a
   completed object.

### 5.10. Opening, Recovery, and Keyless Inspection

A keyed Reader MUST parse the scalar header, require format value 2, read and
canonically parse the key frame, select the slot matching the supplied epoch
id, and unwrap the DEK using Section 5.3. It then derives the
keys, authenticates and parses metadata, rederives the salt, decrypts exactly
`N` chunks, verifies plaintext size and SHA-256, verifies the footer and zero
fill, and requires end of input. It MUST release no unauthenticated chunk.

After whole-object authentication, a recovery implementation SHOULD validate
the inner RAO stream and MUST compare its `REMANENCE.object_id` with the scalar
header before publishing restored members. Catalogless recovery is possible:
an object plus one matching recipient private key is sufficient. Recovery
output SHOULD be staged and published only after complete success.

A Keyless Verifier MAY parse the header and key frame, compute
`stored_digest`, and validate Section 5.8 geometry, footer, and fill. It MUST
describe this result as public structural consistency, not cryptographic
authenticity or provenance.

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

Inner body block `b` is AEAD chunk `b` (Section 5.7). Let
`K = key_frame_len` and `F = 128 + K + metadata_frame_len`:

```text
cipher_offset(b) = F + b × (C + 16)
cipher_len       = C + 16
nonce counter    = b
final_flag       = 0x01 if b == object_chunk_count − 1, else 0x00
```

Here `object_chunk_count` is the **envelope-wide** chunk count `N` of Section 5.7
(`N = plaintext_size / chunk_size`), which governs AEAD finality — not the
per-file `chunk_count` of Section 4.6.4. The Restorer fetches each stored range
`[cipher_offset(b), cipher_offset(b) +
C + 16)` for `b` in `b_first ..= b_last` (a single contiguous stored range,
since consecutive chunks are adjacent), authenticates and decrypts each chunk
with the nonce above, concatenates the plaintexts, and slices the requested
bytes per Section 6.2. Finality comes from the formula — that is, from the
authenticated `plaintext_size` — never from probing. A Restorer MUST NOT
release plaintext from a chunk whose tag failed (Section 5.10). A Restorer MAY
release each chunk's plaintext as that chunk is authenticated (memory-bounded
streaming); a chunk whose tag fails MUST abort the range read, and the
Restorer MUST signal the failure to its Consumer so that an
already-released, individually-authenticated prefix is never mistaken for a
complete range. A partial release is not a successful range read. `chunk_count`
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
   actually sealed and fails — footer unwritten — on mismatch (Section 5.9
   step 7). It computes the encrypted copy's `stored_digest` over its own
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
  exact `object_metadata` inventory validation (Section 4.7.6), final-fill
  zero check (Section 4.8), and
  report-all-nonconformities (not first-error-only), plus a `stored_digest`
  comparison against the catalog value when available.
- **Encrypted copy (keyed)**: Section 5.10 in full (header, metadata, salt
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
`stored_digest`, `stored_size_bytes`/block count, `chunk_size`,
`format_version`, `metadata_frame_len`, `key_frame_len`, and the recipient
epoch ids actually present.

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
`manifest_sha256`). An **encrypted** object's row carries no manifest anchors
— only the public envelope geometry and recipient epochs; [REMPARITY] states this as a
normative rule of its bootstrap rows, and a Writer producing the tape
binding MUST honor it. Manifest size and location are
structural facts about confidential content — manifest size correlates
directly with member count — and the bootstrap is plaintext on the very tape
the envelope protects; the envelope's authenticated metadata already anchors
the manifest more strongly than an external digest could (Section 7.1).
The exact bootstrap encoding belongs to the REM-PARITY specification and is
not duplicated here. Catalogless recovery begins with stored block 0: it
reveals `object_id`, format version, and the length of the adjacent key frame,
from which recipient epochs are read. With the corresponding key, the full self-describing object — the manifest located by sequential
decryption rather than by anchor — is recovered, an acceptable cost on a recovery path over
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
   retried on the recovered stored bytes (Section 5.10 fail-closed rule).
2. Within one object's tape file there are no parity or bootstrap blocks; the
   object's stored blocks are contiguous (stored `BodyLba` 0..N−1). Parity
   epochs span objects; sidecars land between tape files. None of this is
   visible in, or part of, the object's stored bytes.
3. An object is *committed* when the parity layer's durable object-commit
   operation completes ([REMPARITY]); neither representation defines an
   in-band commit marker (the envelope footer detects incomplete writes; it
   is not a commit barrier).
4. The bootstrap row carries, per encrypted object, the public envelope fields
   required by Section 8.2 and never manifest anchors. This is an additive bootstrap
   schema requirement, not a change to the parity construction.

## 10. Versioning and Extensibility

Three independent fields are deliberately not interchangeable:

1. **Specification version 1.0** identifies this publication. It normatively
   describes every format below; it is not stored in an object.
2. **Plaintext stream schema version** is the text
   `REMANENCE.schema_version`: `1.0` when no xattr is preserved and `1.1` when
   any xattr is preserved; `ext` containers and the `object_metadata` inventory
   do not affect the gate. Both use `REMANENCE.format_id = rao-v1` and manifest
   integer `schema_version = 1`.
3. **Encrypted-envelope `format_version`** is the byte at header offset
   `0x06`: value `2` identifies HPKE-wrapped-DEK encryption. Value `1` is
   permanently reserved, MUST never be accepted, and MUST never be
   reassigned. The field does not indicate the stream schema and is unrelated
   to the decimal specification version.

A Reader of `rao-v1` gates on stream-schema major 1 and ignores unknown pax
keywords, manifest keys, and extension-container keys. Extensions MUST NOT
change an existing field's meaning, alignment, entry semantics, compression
or encryption gates, or any other rule enforced by this document. Such a
change requires a new stream `format_id`.

For the `RAO1` envelope magic, only the header form in Section 5.2 is valid:
format value `2`, HPKE wrap suite `0x01`, and a key frame. Unknown format or suite values are hard errors, not
negotiation. A future envelope change not expressible by ignorable metadata
requires a new format value or magic and a successor specification.

A Repacker (Section 2.2) MUST reproduce unknown manifest keys and unrecognized
extension-container members unchanged under the Section 4.7.1 canonical
encoding; ignore-on-read does not license drop-on-rewrite. For symmetry a
Repacker MUST likewise re-emit all unknown pax keywords unchanged
(strengthening the Section 4.4.3 SHOULD to a MUST for the preserving-rewrite
case). A Repacker that recognizes the `xattrs` map and selectively strips
attributes is performing a declared policy action, not a transparent rewrite,
and thereby changes `plaintext_digest`. Because every extension is ancillary
(Section 4.7.5), a minimal Consumer that ignores all extension data and
recovers payload bytes and structure remains conformant for the roles it
claims (Section 14).

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
MissingManifest           object EOF reached with no _remanence/manifest.cbor entry (Section 4.9;
                          non-fatal warning in restore mode, rejection for a Verifier per 7.4)
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
UnsupportedFormatVersion     format_version is not 2
InvalidSuite                 suite_id is not 0x01
InvalidChunkSize             chunk_size is zero or not a multiple of 512
ReservedBytesNotZero         flags or reserved bytes are nonzero
InvalidWrapSuite             wrap_suite is not 0x01, including reserved suite 0
InvalidKeyFrameLength        key_frame_len violates the version gate or bounds
InvalidKeyFrame              malformed or non-canonical RAOK frame
RecipientEpochMismatch       no slot matches the supplied private-key epoch
HpkeFailed                   HPKE key parsing, setup, wrap, or unwrap failed
EntropyUnavailable           OS-backed randomness could not be obtained
InvalidSalt                  all-zero hkdf_salt
SaltDerivationMismatch       header hkdf_salt differs from Section 5.5 derivation
InvalidObjectIdField         object_id field all-NUL, interior NUL, or invalid
                             UTF-8 (reader-side; a >64-byte object_id is
                             rejected at sealing time as InvalidInput, 5.2)
MetadataFrameLengthInvalid   metadata_frame_len outside [17, 16 MiB]
UnexpectedEof                declared header, metadata frame, footer, or fill bytes missing
MissingFinalChunk            EOF within the payload frame before an authenticated final chunk
AeadAuthenticationFailed     metadata or chunk tag verification failed
InvalidCborEncoding          metadata frame plaintext is not valid metadata-profile CBOR (5.6)
MissingRequiredMetadataField required metadata key absent
InvalidMetadataField         metadata key wrong type/value; plaintext_size zero, not a
                             multiple of chunk_size, or implying overflow
PlaintextDigestMismatch      computed canonical-bytes digest differs from plaintext_digest
PlaintextSizeMismatch        computed canonical-bytes size differs from plaintext_size
InvalidFooter                bytes at footer_offset are not the footer
FillNotZero                  nonzero byte in the post-footer fill
TrailingData                 bytes beyond stored_size_bytes in a whole-object input
InnerObjectMismatch          decrypted stream's object_id / chunk_size / format gates
                             disagree with the envelope header (Section 5.10)
InvalidInput                 sealing input violates Section 5.9 step 1 (writer-side)
Io                           underlying I/O failure that is not a format violation
```

The recommended detection orders are Section 5.2 (header) and Section 5.10
(whole object). For multi-fault inputs any applicable error is conformant;
test vectors are single-fault by construction (Section 13).

## 12. Security Considerations

### 12.1. Per-Object Key Uniqueness Is Structural

Every seal MUST use a fresh uniformly random 32-byte DEK and fresh HPKE
encapsulation randomness for every recipient, following [RFC9180] Section
9.2.3. Entropy failure is fatal. The DEK feeds both the salt derivation and
the header-bound key schedule, so independent seals of identical canonical
bytes produce independent envelope keys and normally different stored bytes.

Within one envelope, `header_hash` binds the complete scalar header and key
frame, including `object_id`, recipient slots, encapsulations, and wrapped-DEK
ciphertexts. The derived salt also binds `object_id`, `plaintext_digest`, and
the metadata hash. Reusing an `object_id` is still forbidden by the object
model, but it does not collapse distinct seals onto one key because their
DEKs are independently random. The remaining catastrophic cases require a
DEK collision, a primitive break, or a defective entropy source; the Sealer's
fail-closed obligation is therefore part of nonce safety, not merely an
availability policy.

The deterministic entry point used to generate Section 13 vectors injects a
fixed DEK and seeded encapsulation stream solely for reproducible conformance
artifacts. It is not a production sealing mode; production callers MUST use
operating-system entropy as required by Section 5.4.

### 12.2. Key Separation Is Required for Nonce Safety

The metadata nonce (12 zero bytes) is byte-identical to the nonce of a
non-final payload chunk 0. The construction is safe only because
`metadata_key` ≠ `payload_key`. Any change that unifies the keys converts this
coincidence into real nonce reuse. A future revision MUST NOT merge the
metadata and payload keys.

### 12.3. Binding Without AAD

Both AEADs use empty AAD. Object identity and chunk position are bound
structurally: the scalar header and the complete key frame are bound
through `header_hash` in the key derivation, and the chunk
index and finality are bound through the nonce. Cross-object splicing fails
because the keys differ; intra-object reordering, duplication, or truncation
fails because the nonce (index, finality) or the missing final chunk fails
authentication. Re-binding the same facts as AAD would add bytes and a second
mechanism without adding security.

### 12.4. Fail-Closed

A failed chunk or metadata tag MUST stop processing without releasing that
chunk's plaintext (Sections 5.10, 6.3); a failed seal MUST NOT produce a footer
(Section 5.9); a parity or CRC failure on stored blocks is repaired by the
parity layer before decryption is retried. Partial plaintext is never emitted
as success; streamed output is not valid until the whole-object open succeeds.

### 12.5. Confidentiality Boundary and Size Leakage

An encrypted copy reveals, by design: that it is an RAO object; the format and
suites; `chunk_size`; recipient epoch ids and labels; the
salt; public frame lengths; `object_id`; and its stored length — from which the
**exact** `plaintext_size` and `chunk_count` are derivable by the Section 5.8
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
fields of Section 8.2 and no manifest anchors ([REMPARITY]). It MAY carry
`object_id` ([REMPARITY] bootstrap key 4), which is itself a public envelope
fact (listed above) and so adds no leakage; the envelope header in stored block 0
remains the authoritative source of `object_id` for an encrypted object. The tape
itself therefore leaks nothing beyond the Section 5.8 header-and-size facts.

### 12.6. Plaintext Copies Are Not Self-Authenticating

A plaintext RAO object provides integrity plumbing, not authentication: an
attacker who can rewrite the medium can rewrite payloads, pax hashes, and the
manifest consistently. A lone plaintext object whose hashes verify internally
proves only self-consistency. The trust anchor is external — the catalog's
`stored_digest` and, on tape, the bootstrap's parity-protected
`manifest_sha256` (plaintext rows, Section 8.2). Encrypted copies are
cryptographically authenticated under keys derived from the wrapped DEK —
subject to Section 12.7.

### 12.7. Non-Committing AEAD

ChaCha20-Poly1305 is not key-committing: a party holding multiple candidate
keys can in principle construct equivocal ciphertext [AEAD-COMMIT]
[PART-ORACLE]. `stored_digest` does not prevent this because equivocation uses
one byte string. RAO therefore claims confidentiality and self-consistency,
not writer identity or provenance. The complete recipient frame is bound, but
possession of recipient public keys still permits fabrication of a new
internally valid object. Deployments requiring provenance need an independently
authenticated or signed external manifest.

### 12.8. Key Rotation and Epoch Longevity

Recipient rotation affects newly sealed objects. Because the key frame is
included in `header_hash`, rewrapping without resealing is forbidden. An epoch
private key MUST NOT be destroyed while any live object's key frame references
it. The `reseal` operation opens an existing envelope with a matching
private key and seals the identical canonical bytes to a new recipient set;
it is therefore a full re-seal, not a key-frame rewrite. Re-sealing
preserves `object_id`, `chunk_size`, the canonical bytes, and
`plaintext_digest`, but uses a fresh DEK and salt and changes the encrypted
stored bytes and `stored_digest`.

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

**Native path-mapping preflight.** Section 4.6.6 makes an entry path a clean
`/`-separated relative path, but that grammar is validated against POSIX
semantics only. On a non-POSIX target filesystem the same bytes can denote
something else: on Windows a component such as `..\outside` embeds a separator
the RAO grammar never inspected, and a value like `C:\x` or `\\host\share\x` maps
to a drive-relative or UNC absolute path; case-folding and Unicode normalization
(e.g. NFC/NFD, or Windows case-insensitivity) can also collapse two
RAO-distinct entry paths onto one native target. A Restoring Consumer that maps
entry paths onto a native filesystem MUST therefore, before materializing any
entry, resolve the entry's native-normalized destination (applying the target's
separator, case-fold, and Unicode-normalization rules) and MUST reject or report
— never silently overwrite — any entry whose native destination escapes the
restore root, resolves to an absolute, drive-relative, or UNC path, or collides
with a destination already produced by another entry in the same object. This
preflight is in addition to, not a replacement for, the symlink and traversal
discipline above. Framing-layer acceptance of a path is a necessary check, not a
sufficient safety claim. Decrypting an envelope grants no exemption: the inner stream is
parsed and restored under the same rules. Stock tar extraction has its own
security model; RAO's standard-tool fallback is faithful, not inherently
sandboxed.

Preserved xattrs are equally untrusted. Attributes such as Linux
`security.capability`, `security.*`, `trusted.*`, and POSIX ACL attributes can
change privilege or access-control state — a restored `security.capability`
is a privileged binary. A Restoring Consumer MUST restrict applied attributes
to the `user.` namespace unless explicit operator policy names additional
namespace prefixes; attributes outside the effective allow-list MUST be
skipped and reported (names only — values MUST NOT be logged), never applied.
It MUST treat values as opaque bytes. A Restoring Consumer MUST NOT write an
attribute through an interface that follows a symbolic link at the final path
component; for entries that are symbolic links it MUST use a link-targeting
interface or skip and report the attribute. Skips are policy outcomes, not
errors; genuine application failures MUST still surface per Section 4.7.3.

The same disposition governs `ext` extensions (Section 4.7.5): a Restoring
Consumer applies only the `user.` portable core by default; every non-`user.`
namespace and every extension — recognized or not — is carried and, when
reported, reported by name only, and is applied on restore only when explicit
operator policy names it. No registered disposition applies an extension-tier
item by default in version 1.0. A Restoring Consumer that reports skipped or
applied names MUST NOT log their values.

### 12.11. Envelope Threat Model and Secret Handling

The encrypted-envelope confidentiality claim is time-scoped:

| Attacker capability | Can read or do | Cannot read, assuming uncompromised primitives and custody |
| --- | --- | --- |
| Steals only object media | Public header/key-frame facts and exact size | Metadata and payload |
| Compromises the sealing host at time T | Host plaintext; in-memory keys; objects being or later sealed while compromise persists; substitute future recipient keys if pinning also fails | Earlier objects whose DEKs and recipient private keys are absent |
| Holds one recipient private key | Every object wrapped to that epoch | Objects not wrapped to that epoch |
| Holds only recipient public keys | Create new internally valid objects; observe public facts | Existing-object plaintext |

The last row is why catalogless recovery establishes confidentiality and
self-consistency, not provenance. Recipient public keys MUST be pinned by an
independent custody process if key substitution is in scope. At least two
distinct-custody recipients reduce loss risk but do not impose a cryptographic
threshold: either private key decrypts alone.

Implementations SHOULD keep DEKs, derived keys, recipient private
keys, HPKE ephemeral secrets, and RNG state in non-cloneable or otherwise
minimally copied containers and MUST zeroize mutable secret buffers promptly
after use. Secrets MUST NOT appear in logs, diagnostics, command lines, core
dumps, or durable plaintext staging. Whole-object recovery SHOULD stage
plaintext on suitably protected storage and publish it only after full
authentication; deletion cannot guarantee erasure on copy-on-write or flash
media.

**Post-quantum confidentiality (a stated limitation).** The version-1 envelope
wraps DEKs with HPKE over DHKEM(X25519) (Section 5). X25519 key agreement is not
post-quantum secure: an adversary who records stolen encrypted media today can
recover the wrapped DEKs once a cryptographically relevant quantum computer
exists — a *harvest-now, decrypt-later* exposure that is acute precisely because
archival media is long-lived and confidentiality must hold for decades.
Deployments holding long-lived secrets under this format MUST treat the
confidentiality window as bounded and adopt a **resealing policy**: re-encrypt
affected objects under a post-quantum-secure KEM before a deployment-stated
deadline. Merely adding a second, post-quantum recipient slot alongside the
X25519 slot does **not** help — the weaker slot still unwraps the same DEK
(weakest-slot property), so a genuine defense requires replacing the KEM or a
true hybrid. A future minor version is expected to define an X25519 + ML-KEM
hybrid KEM (FIPS 203; the construction of RFC 9958) as the resealing target;
version 1.0 states the limitation and the resealing obligation but does not yet
specify the hybrid.

### 12.12. Disclosure in Published Plaintext Objects

A plaintext RAO object provides integrity plumbing (Section 12.6), not
confidentiality: its manifest is readable with any CBOR tool (Section 4.10).
Publishing a plaintext object discloses, at minimum: every entry path and
(possibly absolute or dangling) symlink target; the directory tree and
hardlink topology; file sizes and the file count; `mtime` and `executable`
values; `file_id`, `object_id`, `caller_object_id`, `write_timestamp`, and
`chunk_size`; every captured attribute value; and the `object_metadata`
inventory itself, which names the non-`user.` namespaces and extensions
present (revealing, for example, macOS or Windows origin). The encrypted
representation places the manifest inside the encrypted,
per-chunk-authenticated payload (Sections 5.1, 5.7) and does not have this
plaintext-disclosure exposure. Reviewing a plaintext object before publication
is a deployment (workflow) obligation in the sense of Section 7.3; the
Verifier-validated inventory (Section 4.7.6) is the intended first-pass
screening surface, but does not itself bound value-level disclosure. The
standard-tool recovery path (Section 4.10) inherits the host tool's security
model — it restores symlinks faithfully — and the format's protection there is
limited to keeping privilege-changing metadata (ownership, setuid/setgid mode,
extended attributes) in a form no standard `tar` applies to target files; it
is not a sandbox.

## 13. Test Vectors

Static test vectors are distributed alongside this specification, each with a
manifest entry recording inputs, the expected values pinned below, and — for
negative vectors — the expected Section 11 error name. Vectors use small `chunk_size`
values (e.g. 4096) so full object byte streams are practical to pin; at least
one vector MUST use `DEFAULT_CHUNK_SIZE`.

The authoritative companion archive is `remanence-test-vectors.tar`, SHA-256
`f4e4331c14e67c059d1292f54e14efd8408c7d41364d2dba7f8e7567aa16c2a6`. This archive supersedes the earlier
`32fe2a7947b74e5c8abbaad4e83e85f7deebc827d0aa8ccee8197fcc9c6cd6da`: every
prior entry is byte-identical and only additive entries (the metadata,
extension-container, object-inventory, and parity tie-break vectors) are
new.
Its `MANIFEST.tsv` inventories every contained vector manifest and generated
artifact, `CHECKSUMS.sha256` authenticates them, and the included `verify.py`
checks the archive without a source checkout. It contains plaintext and xattr
positive objects, HPKE-envelope positive objects, and envelope negative
manifests. The archive's checksums, rather than abbreviated values
in this prose, are the byte-identity authority. Payload digests are
independently checkable with `sha256sum`.

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
set (the cross-implementation determinism gate, Section 4.7.1); a
**portable-core-only object** (`user.` only, empty `object_metadata`, and
`REMANENCE.schema_version` pinned); an **object with a non-`user.` attribute
and a correct inventory**, whose default restore reports it as not applied
(carry-only) and omits its value from output; an **object with an unknown
reverse-DNS `ext` member**, for which a minimal Consumer recovers payloads and
ignores the member and a Repacker reproduces it under canonical encoding; and
a **combined non-`user.` attribute and `ext` member** with the two-array
inventory pinned exactly. For each, the
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
`manifest_sha256`; `plaintext_digest` = `stored_digest`. Exact values are
pinned in the companion archive.

### 13.3. RAO-TV-E2 — Encrypted Twin of RAO-TV-P1

RAO-TV-E2 seals the exact RAO-TV-P1 canonical bytes. Its manifest records the
complete inputs and derivation chain; the essential fixed inputs are:

| Input | Value |
| --- | --- |
| DEK | byte `7d` repeated 32 times |
| HPKE RNG seed | byte `c3` repeated 32 times |
| Slot 0 | index 0; epoch id `61` repeated 16 times; label `archive-2026-01`; private key `51` repeated 32 times; public key `ad908a8a708aca07588cda7c4ed3e44d4966a80a9abb2f1e4bbac53c67414e34` |
| Slot 1 | index 1; epoch id `62` repeated 16 times; label `recovery-2026-01`; private key `52` repeated 32 times; public key `f68b05ba03f7185e1ba88878682f8dd0b15158f6050889c9481d79c2d7d2fa07` |
| Envelope metadata | The four required keys of Section 5.6 |

The private keys, DEK, and seed are public test material, not operational
examples. Expected geometry is:

| Quantity | Expected value |
| --- | --- |
| `plaintext_size` `P` | 20480; `chunk_count` = 5 |
| Key frame | `K` = 232 bytes; two recipient slots |
| Metadata plaintext | 50-byte map `{0: 1, 1: 20480, 2: "sha256", 3: <32 digest bytes>}` |
| `metadata_frame_len` `M` | 66 |
| Payload frame | bytes 426–20985 (5 chunks × 4112) |
| `footer_offset` | 20986; footer + 3574 zero-fill bytes |
| `stored_size_bytes` / blocks | 24576 / 6 |

The manifest pins each recipient's `enc`, wrapped-DEK ciphertext, HPKE shared
secret, HPKE AEAD key and base nonce; the metadata plaintext and hash; derived
`hkdf_salt` (`ctr = 0x00`), `object_secret`, `metadata_key`, and `payload_key`;
the exact header, key frame, metadata frame, and `header_hash`; payload-frame
SHA-256; `stored_digest`; and `plaintext_digest`. Required equality: the
`plaintext_digest` MUST equal RAO-TV-P1's `stored_digest`.

The reference generator calls `seal_deterministic_for_test_vectors` with the
fixed DEK and seed, generates the object twice, and requires byte equality
with `rao/objects/rao-tv-e2.rao`. This deterministic-generation hook exists
solely to reproduce conformance artifacts. Production sealing uses a fresh
DEK and operating-system-backed HPKE randomness as required by Section 5.4.
The independent verifier implements only the OPEN direction from this prose
using generic cryptographic primitives; it unwraps both slots and verifies the
canonical bytes, metadata digest, manifest, and per-file digests.

### 13.4. RAO-TV-D1 — Default Chunk Size

One vector MUST use `DEFAULT_CHUNK_SIZE`. Inputs: `chunk_size` 262144;
`object_id` `00000000-0000-4000-8000-000000000002`; `caller_object_id`
`rao-tv-d1`; `write_timestamp` `2026-01-01T00:00:00Z`;
`metadata_preservation` `minimal`; `manifest_file_id`
`00000000-0000-4000-8000-0000000000fe`; one file `v.bin`, `file_id`
`00000000-0000-4000-8000-000000000012`, contents 262145 bytes with byte `i` =
`i mod 256` (expected `file_sha256`
`c35991ad254f48ff8b02becb9f0cc56581e86a0b477b13e5ebb0030a3b91c848`,
`chunk_count` 2), represented both as plaintext and as a two-recipient HPKE
envelope. The encrypted half uses DEK byte `5d` repeated 32 times, HPKE RNG
seed byte `a7` repeated 32 times, and the two recipient keypairs recorded in
its manifest. Pinned outputs as in 13.2/13.3
(digests only for the large streams; exact bytes for header, metadata frame,
and manifest).

### 13.5. Xattr and HPKE Component Vectors

`rao/objects/rao-tv-xattrs.rao` and its manifest pin an xattr round trip. The
entry `tagged.txt` carries `user.comment` with bytes `62 6c 75 65` (`blue`)
and `user.remanence.color` with bytes `01 02 ff`; the global stream schema is
`1.1`. Its entry container is equivalent to:

```text
{"xattrs": {
  "user.comment": h'626c7565',
  "user.remanence.color": h'0102ff'
}}
```

The `plain.txt` entry carries an empty container. The companion manifest pins
the exact deterministic CBOR, layout, `manifest_sha256`, and
`stored_digest`. The no-xattr writer path remains schema `1.0` and emits empty
containers as required by Section 4.7.3.

The archive's `negative-key-frame.json` pins the complete header,
key-frame, HPKE-tamper, Sealer-slot, and Reader-slot policy matrix described in
Section 13.6. The implementation's positive
component vector fixes `object_id = object-a`, epoch id `03` repeated 16
times, label `safe-2026`, slot 0, DEK `09` repeated 32 times, and a deterministic
test-only HPKE entropy draw of `42` repeated 32 times. Its exact outputs are:

```text
key frame =
52 41 4f 4b 01 00
03 03 03 03 03 03 03 03 03 03 03 03 03 03 03 03
09 73 61 66 65 2d 32 30 32 36
ae 3b f1 cd 87 c2 d2 ed 25 af 4a 1a 23 9e ed 04
a9 90 f0 0e 74 03 e4 c8 06 59 27 de 01 0f d1 7a
fd 48 22 7f 58 c8 a2 b4 ac 3e b0 b2 24 b1 18 5e
85 8c 7a 46 44 f9 6a 70 67 d2 c2 d3 2d 1c 67 da
d5 73 cb a8 d9 4b 66 8c a2 ab 98 b6 ca 12 a1 8c

enc = ae3bf1cd87c2d2ed25af4a1a239eed04a990f00e7403e4c8065927de010fd17a
ct  = fd48227f58c8a2b4ac3eb0b224b1185e858c7a4644f96a7067d2c2d32d1c67da
      d573cba8d94b668ca2ab98b6ca12a18c
```

The deterministic entropy source exists only to make this HPKE component
vector reproducible. Conforming production sealers use OS-backed fresh
randomness as required by Sections 5.3 and 12.1.

### 13.6. Negative Vectors

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
sharing a `file_id`. Additive negative vectors cover an inventory that
disagrees with the entries (a non-`user.` attribute is present but the
inventory is empty or wrong), which MUST produce `ManifestInvalid`; a
non-canonical `ext` value, which MUST produce `Cbor`; and a manifest tamper
with constant payload (a repointed `path`, swapped `file_sha256`, or altered
`first_chunk_lba`), which pins a distinct `plaintext_digest` and, with an
anchor present, MUST produce `ManifestDigestMismatch`. Each additive negative
vector pins the typed Section 11 error name and names the affected digest,
`plaintext_digest`, which equals `stored_digest` for a plaintext copy. A
restore-report vector reaches EOF without a manifest and asserts the typed
`MissingManifest` report rather than silent absence.

**Envelope.** Header: wrong magic; `header_len` ≠ 128; unsupported format
values (including the permanently reserved value 1); unknown `suite_id`;
`chunk_size` 0 and `chunk_size` not a multiple of 512; nonzero `flags`;
nonzero bytes in either reserved region; unknown or reserved `wrap_suite`;
suite 0 with a nonempty frame; HPKE suite with zero, undersized, or oversized
`key_frame_len`; all-zero `hkdf_salt`;
all-NUL `object_id` field; interior-NUL `object_id`; non-UTF-8 `object_id`;
`metadata_frame_len` 16 and `metadata_frame_len` > 16 MiB. Cryptographic
binding: a flipped salt bit (structurally valid → `AeadAuthenticationFailed`);
a structurally valid key-frame label, encapsulation, wrapped-DEK ciphertext,
slot insertion, or slot removal tamper; a
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
same input is advisory — Section 5.8). Inner cross-checks (defective-sealer
harness): inner `object_id` differing from the header; inner `chunk_size`
differing; inner `REMANENCE.encryption` ≠ `none` (`InnerObjectMismatch`).
Writer-side: sealing input with `P` not a multiple of `C`; `object_id` > 64
bytes; recipient counts 0, 1, and greater than 8; duplicate epoch ids;
non-canonical slot order; or entropy failure.

Key-frame structure additionally covers slot counts 0 and 9, duplicate slot
indices, misordered slots, duplicate `recipient_epoch_id` values across
distinct slots, internal slot truncation, trailing frame bytes, malformed
`RAOK` magic, malformed encapsulation, and a wrong recipient private key. A
positive case opens a structurally valid one-slot object, recording the
policy asymmetry: Sealers MUST emit at least two slots, while Readers accept
one through eight.

## 14. Conformance

An implementation conforms only for the roles it claims. A conforming Writer
implements the canonical stream and every feature it emits. A conforming
encrypted Sealer and Reader MUST implement the complete format-2 header,
key-frame, HPKE, key schedule, and mode-separation rules. A Reader MAY decline xattr restore,
but it MUST preserve file-byte recovery, ignore the extension safely, and
report that the attributes were not applied.

Conformance evidence MUST include:

1. byte-exact agreement with the applicable positive objects and manifests in
   the Section 13 archive;
2. the applicable typed rejects in the negative manifests, including the
   key-frame set;
3. GNU tar, bsdtar, and Python `tarfile` extraction equality for plaintext
   objects;
4. authenticated range recovery across a chunk boundary and in the final
   encrypted chunk;
5. failure without a completion footer on injected size, digest, entropy, and
   I/O failures applicable to the claimed mode;
6. whole-object keyed verification and keyless structural verification with
   the claims kept distinct;
7. reconstruction of one catalogless encrypted object using only object bytes, a
   matching recipient private key, this specification, and generic
   cryptographic libraries; and
8. the applicable portable-core, extension-container, object-inventory,
   carry-only restore, Repacker-preservation, and manifest-tamper vectors of
   Section 13.

The archive SHA-256 in Section 13 identifies the frozen vector distribution.
Changing an existing entry's byte encoding or expected result requires a
successor specification or erratum; adding new entries is permitted and
advances the archive digest.

## 15. IANA Considerations

The identifiers this specification defines — the `rao-v1` stream format
identifier, the `REMANENCE.` pax keyword namespace, the `RAO1` envelope magic,
`format_version` value 2 (with value 1 permanently reserved), the `suite_id`
value `0x01`, the `wrap_suite` value `0x01`, the `"xattrs"` preservation key,
the `ext` indirection key, and the `object_metadata` inventory keys
(`attribute_namespaces`, `extensions`) — are assigned by this document and
governed by its versioning rules (Section 10).

This document establishes no IANA registry. Extension names (Section 4.7.5)
use permissionless reverse-DNS naming and require no central allocation; a
community-maintained advisory list MAY record registered short names, but is
not a precondition for conformance and does not bear on the carry-only restore
default (Section 12.10). Reverse-DNS extension names apply to manifest
extension containers only and MUST NOT appear as pax keywords.

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
- [RFC9180] — Barnes, R., Bhargavan, K., Lipp, B., and C. Wood, "Hybrid
  Public Key Encryption", RFC 9180, February 2022,
  <https://www.rfc-editor.org/info/rfc9180>.
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
  STREAM construction reference (informative provenance; Section 5.7 is
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
- [PREMIS] — PREMIS Editorial Committee, "PREMIS Data Dictionary for
  Preservation Metadata", Version 3.0, November 2015, Library of Congress,
  <https://www.loc.gov/standards/premis/v3/>.
- [OAIS] — Consultative Committee for Space Data Systems, "Reference Model for
  an Open Archival Information System (OAIS)", CCSDS 650.0-M-3, Issue 3,
  December 2024, <https://public.ccsds.org/Pubs/650x0m3.pdf>.
- [REMANENCE] — "Remanence", the reference implementation of this
  specification: an open archival tape stack (tape library control, tape
  I/O, parity, and this object format),
  <https://github.com/archivetechie/remanence>.

---

## Appendix A. Worked Example (Informative)

This appendix derives the Section 13.2/13.3 expected values, exercising the
alignment equation, the manifest sizing, and the envelope geometry. It is
informative; the frozen companion archive is the conformance authority.

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

**RAO-TV-E2.** `P` = 20480, `C` = 4096 → `chunk_count` 5. Metadata plaintext:
`a4 00 01 01 19 50 00 02 66 ... 03 58 20 ...` = 1 + 2 + 4 + 8 + 35 =
**50 bytes** → `M` = 66. The two-slot key frame has `K` = 232. Payload frame
= 20480 + 80 = 20560 bytes at offset 128 + 232 + 66 = 426;
`footer_offset` = 20986; stored pre-fill length 21002; fill 3574 →
**`stored_size_bytes` 24576 = 6 blocks**. The metadata zero nonce is
byte-identical to payload chunk 0's nonce here (chunk 0 is non-final, nonce
`00…00 00`) — exactly the collision that the metadata/payload key separation
renders harmless (Sections 5.6, 12.2).

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
sensitive precisely for the copies that leave the operator's custody, and
keyless rebuild paths remain available wherever a catalog or a plaintext
representation of the object exists.

### B.6. Stored bytes are a block multiple; the fill is inside them

An encrypted object's stored bytes include zero fill from the footer to the
next `C` multiple, covered by `stored_digest` and parity. This gives
byte-stable fanout (one byte string, identical on tape, disk, and object
store, with one `stored_digest`), uniform block geometry for parity, no
backend-specific framing, and — because chunk counts are spaced `C + 16` apart
while the fill absorbs at most `C − 1` — a keyless-derivable geometry that
turns footer verification into an exact positional check (Section 5.8). Bytes
beyond `stored_size_bytes` are rejected (`TrailingData`).

### B.7. Two metadata layers, two CBOR profiles

The **manifest** (manifest-profile CBOR, text keys, inside the canonical
bytes) is the object's per-file index and is consequently encrypted in the
encrypted representation. The **envelope metadata frame** (metadata-profile
CBOR, integer keys) carries only what decryption itself needs
(`plaintext_size`, `plaintext_digest`). They are not merged: folding per-file
metadata into the envelope frame would move the index outside the
self-describing object and bloat a frame whose smallness bounds hostile input.
Both profiles use deterministic-CBOR validation (Sections 4.7.1 and 5.6).

### B.8. Empty AAD

Both AEADs use empty AAD because the bindings are already structural: the
scalar header and the key frame are bound through `header_hash` in
the key derivation, and chunk index and finality are
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

`hkdf_salt` is derived from the per-object DEK, object identifier, content
digest, and metadata bytes (Section 5.5), never drawn directly from a random
number generator. The envelope is nevertheless deliberately randomized by
its fresh DEK and HPKE encapsulations, while the salt remains reproducibly
derived once that DEK is known. This separates the entropy input from the
wire salt and binds the salt to the object facts authenticated on open.

### B.12. Envelope metadata is fixed and bound into the salt

If variable optional metadata existed, resealing the same object with
different metadata of the same encoded length could reproduce the same
`metadata_key` while the zero metadata nonce encrypted a *different* plaintext
— the nonce reuse [RFC8439] forbids. Two layers prevent this: the current
format defines no optional metadata keys (the frame is a deterministic
function of the canonical object), and SHA-256 of the metadata plaintext is bound into the
salt derivation and verified on every keyed open, so a future metadata
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

## Appendix C. Revision History (Informative)

**2026-07-22.** Added the extension-tier / `ext` container (§4.7.5), the
object metadata inventory (§4.7.6), the carry-only restore default for
non-`user.` metadata (§12.10), and the plaintext-disclosure considerations
(§12.12). The set of valid pre-increment objects is unchanged; new objects are
tolerated by existing Consumers per §4.7.2 obligation 3.

**2026-07-22.** Section 12.10's extended-attribute restore protections
restored to requirement strength (MUST): namespace allow-list defaulting to
`user.`, skip-and-report for excluded attributes, and no-follow application.
These were requirements in the pre-publication draft, were relaxed to
recommendations during pre-publication drafting to track a then-deficient
reference implementation, and are restored now that the reference
implementation enforces them. The change constrains Restoring Consumer behavior only; the
set of valid RAO objects is unchanged.

Specification Version 1.0 is the first unified publication baseline. It
consolidates the project's internal base-format revision, its additive xattr
revision, and the wrapped-DEK envelope revision. Those earlier documents are
revision history only; this document is the complete normative specification.
REM-PARITY remains the separate companion format layer identified by
[REMPARITY] and is not incorporated here.

## Author's Address

The ArchiveTech Project
Website: https://archivetech.org
Email: specs@archivetech.org
Reference implementation: https://github.com/archivetechie/remanence
