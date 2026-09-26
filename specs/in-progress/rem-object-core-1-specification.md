# REM-OBJECT Core Format 1.0

## Abstract

This document specifies REM-OBJECT Core Format 1.0: a backend-independent byte
format for large archival objects. A REM-OBJECT bundles named file payloads
into one self-describing unit—a constrained POSIX pax tar stream carrying
per-file SHA-256 identities, closed-form byte-range addressing, and a
deterministic CBOR manifest. The canonical object may be stored directly as
the **plaintext** representation or sealed inside the authenticated,
confidential wrapper defined by REM-ENCRYPT.

Both representations share the logical identity `plaintext_digest`; each
stored copy has a representation-independent physical identity,
`stored_digest`, that backends can scrub without interpreting the bytes. The
format is designed for single-pass writing, byte-stable fanout to tape, disk,
and object storage, parity protection over stored bytes, and long-term
recovery from this document and its static test vectors. Canonical plaintext
construction is deterministic.

## Table of Contents

- 1\. [Introduction](#1-introduction)
- 2\. [Conventions and Terminology](#2-conventions-and-terminology)
- 3\. [Object Model](#3-object-model)
- 4\. [Plaintext Representation](#4-plaintext-representation)
- 5\. [Encrypted Representation (in REM-ENCRYPT)](#5-encrypted-representation-in-rem-encrypt)
- 6\. [Partial File Restore](#6-partial-file-restore)
- 7\. [Digests, Integrity, and the Verification Chain](#7-digests-integrity-and-the-verification-chain)
- 8\. [Storage Bindings and Backend Independence](#8-storage-bindings-and-backend-independence)
- 9\. [Relationship to the Parity Layer](#9-relationship-to-the-parity-layer)
- 10\. [Versioning and Extensibility](#10-versioning-and-extensibility)
- 11\. [Errors](#11-errors)
- 12\. [Security Considerations](#12-security-considerations)
- 13\. [Test Vectors](#13-test-vectors)
- 14\. [Conformance](#14-conformance)
- 15\. [IANA Considerations](#15-iana-considerations)
- 16\. [References](#16-references)
- Appendix A. [Worked Example (Informative)](#appendix-a-worked-example-informative)
- Appendix B. [Design Rationale (Informative)](#appendix-b-design-rationale-informative)
- Appendix C. [Revision History (Informative)](#appendix-c-revision-history-informative)
- Appendix D. [Open Items (Informative)](#appendix-d-open-items-informative)
- Appendix E. [Packing Many Small Files (Informative)](#appendix-e-packing-many-small-files-informative)
- [Author's Address](#authors-address)

---

## 1. Introduction

### 1.1. Identifiers and Versioning

| Identifier | Value | Scope |
| --- | --- | --- |
| Document version | 1.0 | This publication only |
| Stream format identifier | `rem-object-v1` | Frozen plaintext-stream wire constant |
| Stream schema version | `1.0` without preserved xattrs; `1.1` with preserved xattrs | `REMANENCE.schema_version` |
| Manifest schema version | `1` | Manifest CBOR field |
| Representations | `plaintext`, `encrypted` | Per-copy catalog value |
| Default file extension | `.rem-object` | Both stored representations |

This document's version does not name any on-tape value. The
`rem-object-v1` identifier is a frozen wire constant, not a document-version
indicator. REM-ENCRYPT independently defines the encrypted representation
and its wire discriminators.

REM-OBJECT and REM-ENCRYPT are numbered together, so the two can be read side
by side. This document omits Section 5, the encrypted representation, which is
in REM-ENCRYPT; REM-ENCRYPT omits Sections 4, 9 and 14, the plaintext
representation, the relationship to the parity layer, and conformance, which
are here. Each document marks a section it omits with a placeholder heading
that names the document holding it. Sections 6, 7, 8, 11 and 12 number their
subsections together in the same way: a number used in both documents names
sections on the same subject, and a placeholder marks a subsection that only
the other document contains. Sections 1, 3, 10 and 13 number their subsections
separately, so the same number there can name unrelated sections; Sections 2
and 16 match one for one. A reference into the other document therefore always
names it, as in “REM-ENCRYPT §6.4”, while “Section 6.4” alone means this
document's section.

**Status of This Document**

| | |
| --- | --- |
| Status | Review draft |
| Version | 1.0.0-draft.4 |
| Date | 2026-09-26 |
| License | CC-BY-4.0 |
| Concept DOI (all revisions of this document) | [10.5281/zenodo.21719158](https://doi.org/10.5281/zenodo.21719158) |
| Reference implementation (informative) | Zenodo concept DOI [10.5281/zenodo.21551570](https://doi.org/10.5281/zenodo.21551570) — software deposit, Apache-2.0 |

**This is a review draft.** It is published for public review and is not yet
frozen. The format it describes is implemented and its conformance vectors are
pinned, so what is under review is not whether the design works but whether
*this text describes it correctly and completely* — an independent
implementation built from these words alone should produce the published
vectors, and where it would not, the text is wrong and we want to know.

**Comments close on 30 April 2027, and the documents freeze on 31 July 2027**,
one year after publication. On that date the finality promise below takes
effect and the change policy governs every revision after it. The three months
between the two dates exist so that changes made in response to review are
published as such, and visible, before the text is fixed. Comments and defect reports are raised as issues on the project repository.
Before reporting, check the live list of known items at
<https://archivetech.org/spec/issues> — it is current, whereas the Open Items
appendix of this document is only a snapshot taken when this revision was fixed.

This document is the publication specification for the REM-OBJECT Core
Format. It is the normative fixed point for the durable canonical object: an
implementation is validated against this document, not the reverse.

No standards body has reviewed or adopted it. It was written by the same
people who wrote the implementation, so the stability of this document is our
own undertaking rather than anyone else's approval.

**Which copy governs.** The concept DOI above is reserved, but no revision of
this document has yet been deposited. Until the first deposit is published,
this repository copy is the public review text but is not a deposited normative
revision. Once a revision is deposited, its deposited text governs. Every other copy — in the project
repository, inside a Remanence source release, on a mirror, or printed — is a
convenience copy. A copy carrying the same version string as a deposited
revision is byte-identical to it or it is defective; where they differ, the
deposit governs. The version string of a deposited revision is never reused
for different bytes, so naming a deposited version names one exact text no
matter which copy you hold. The
reference implementation is informative: where it and this document disagree,
this document is the fixed point, and the divergence
is a defect in the implementation.

**Deciding what a change is.** Every revision of this document is classified
by three questions, asked in order.

1. Does any existing object become invalid, or does the meaning of anything
   already written change? Then it is a **new major version**: a new stream `format_id` (`rem-object-v2`), and a
   separate document. Objects written under this major keep working with
   readers of this major; the two formats coexist. An implementation MAY
   implement two majors at once; it then conforms to each major for the
   artifacts that major governs, and a reader-acceptance clause of an
   earlier major constrains only artifacts written under it.
2. Does a reader conforming only to an earlier revision lose the ability to
   identify a newer object and refuse it cleanly? Then it is also a
   **major**.
3. Does anything an implementation must do change, or is a registry value
   assigned? Then it is a **minor revision** (1.1.0, 1.2.0, …), and it is
   permitted only if all three of the following hold: every object written
   under an earlier revision of this major remains valid; a reader built
   from an earlier revision still reads correctly every object that uses
   only the features defined at or before that revision; and on an object
   that uses a feature it does not implement, that reader identifies the
   object and refuses it with a typed error naming the unimplemented value —
   it never misreads, and never mistakes the object for damage. A minor
   revision whose new value earlier readers must refuse MUST say so in its
   revision-history entry.

Anything else — wording, typographical errors, dead cross-references, and
clarifications that resolve an ambiguity the way conformant implementations
already behave — is an **erratum** (1.0.1, 1.0.2, …). An erratum changes no
obligation and no valid object. One exception is named openly: a correction
to this change-policy text itself is published as an erratum and flagged as
a policy correction in the revision history.

Version numbers are always three-part. A revision published for review before it is frozen carries a `-draft.N` suffix on that three-part core; it names the revision it anticipates and orders before it. The Identifiers table at the head of
this document names the major.minor line; the Version row in this section is
the full revision. Section 10 owns the wire-versioning machinery — the stream `format_id`, the `REMANENCE.schema_version` feature gate, and the manifest schema integer; this policy defers to it and does not paraphrase it. Every published revision of this document is
archived with its own DOI and recorded in the Revision History appendix.

For orientation, the classifications most likely to be proposed for this
format (informative): a new optional `REMANENCE.*` keyword, manifest key, or
extension-container member — minor; a change to any existing field's
meaning, or a new stream `format_id` — major. To identify what defines an
object in hand: read `REMANENCE.format_id`, `REMANENCE.schema_version` and
the manifest `schema_version` (Section 10 names the defining revision of
each value it assigns; every value assigned so far was assigned by document
1.0); every revision of this document is retrievable through the
concept DOI above. Which revision wrote an object is not recorded in it and
is not needed for reading. The Section 13 conformance vectors are anchors of
the revision that generated them; a future revision that needs new vectors
publishes its own archive and cites it by name, DOI and digest, and never
re-pins an earlier one.

The tape binding depends normatively on the REM-PARITY specification
([REMPARITY]), which is under review alongside this document. The tape-binding
clauses of this document (the parity-layer references in Sections 3.3, 4.2,
4.5.1, 4.7, 6.5, 8.2, 9, 12.6) are stable against every 1.x revision of
REM-PARITY, because a REM-PARITY minor revision cannot invalidate a tape or
leave an earlier reader unable to read one. An obligation that a later
REM-PARITY 1.x revision places on its Writer (REM-PARITY §2.2) binds a Writer
claiming that revision; it does not change this document's clauses. The file
and object-store bindings do not depend on REM-PARITY.

### 1.2. Purpose and Design Goals

REM-OBJECT wraps a set of named file payloads into one archival object. The format
originates in **Remanence**, an open archival tape stack that serves as this
specification's reference implementation [REMANENCE]. This document
specifies the format completely, so that it stands alone from any
implementation; the name survives in the format itself only as fixed wire
identifiers — the `REMANENCE.` vendor-keyword namespace and the `_remanence/`
manifest path (Section 4). REM-OBJECT's design goals, in priority order:

1. **Plaintext longevity comes first.** A plaintext REM-OBJECT object is a
   fully valid POSIX pax tar archive. A standard pax-aware `tar` extracts
   every payload byte-correct with no Remanence software present.
2. **Self-description.** Every object carries its own per-file index (the
   CBOR manifest): paths, sizes, SHA-256 identities, and the block address of
   every file inside the object. A catalog can be rebuilt from the medium —
   in the clear for plaintext objects, with the key for encrypted ones.
3. **Closed-form byte-range addressing (partial file restore, PFR).** Any byte range of any member
   file maps to stored byte ranges by arithmetic alone, in **both**
   representations. No scanning, no decompression, no whole-object read.
4. **An encrypted representation without a format fork.** REM-ENCRYPT seals
   the identical canonical byte stream, manifest included, without changing
   the Core object model.
5. **Separation of identities.** The logical plaintext identity
   (`plaintext_digest`) is distinct from the physical stored identity
   (`stored_digest`); backends scrub by the latter without keys.
6. **Deterministic canonical plaintext representation.** Given identical
   inputs and options, every conformant Builder produces the identical
   plaintext stream.
7. **Long-term recoverability.** The format is recoverable from this document
   plus its static test vectors, and — degraded, plaintext representation
   only — from knowledge of POSIX tar alone.

### 1.3. Two Representations of One Object

A REM-OBJECT has a single logical form — a self-describing, bundled,
chunk-aligned pax tar stream (Section 4) — stored in either of two
representations:

- **plaintext**: the bare stream, byte-for-byte; self-describing in the
  clear and extractable by commodity `tar`.
- **encrypted**: that identical stream sealed inside the REM-ENCRYPT
  envelope.

Both representations of one object wrap the identical canonical bytes and
therefore share one logical identity (`plaintext_digest`, Section 3.3). The
REM-ENCRYPT profile provides confidentiality and authentication while preserving
self-description and closed-form range addressing after opening.

### 1.4. Relationship to Adjacent Components

REM-OBJECT is the archival object format of the Remanence tape stack. It owns one
thing — the stored bytes of one object — and leaves the rest to the
components around it:

- **REM-OBJECT Core owns** the canonical object bytes: tar framing,
  alignment, vendor keywords, manifest, identities, and
  representation-independent storage obligations.
- **REM-ENCRYPT owns** the optional encrypted wrapper around those canonical
  bytes.
- **The parity layer** [REMPARITY] owns everything outside the object's
  stored bytes on tape: the tape filemark terminating the object's tape file,
  parity sidecars, block-level CRCs, the beginning-of-tape (BOT) bootstrap,
  and the durable commit barrier. Parity is computed over **stored** bytes —
  ciphertext when the object is encrypted (Section 9).
- **The catalog and restore orchestration** above the format own catalogs,
  object selection, restore policy, and restore-time path sanitization.

### 1.5. Non-Goals

REM-OBJECT performs no compression: the payload workload is already-compressed
media, and whole-stream compression destroys closed-form range addressing (a
later member's offset would depend on decompressing earlier bytes). It defines
no catalog format, no key registry, no network protocol, and no multi-object
container: one object is one archive is one stored byte string. This format
encodes a faithful tree of files — regular files, hardlinks, symbolic links,
and (empty) directories. Device nodes, FIFOs, and sockets are excluded on
principle. They carry no content, being kernel or runtime handles, and
materializing them on restore is a hazard. A conformant Reader therefore
rejects their typeflags (Section 4.3.4). Ownership is deliberately not
preserved (Section 4.3.1); selected POSIX extended attributes are preserved as
specified in Section 4.7.3. Encryption policy and key custody are outside this
document; see REM-ENCRYPT §1.

### 1.6. What this document specifies, and what it does not

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

### 2.2. Conformance Roles

A single implementation may fill several roles.

- **Builder**: produces the canonical plaintext object (Section 4.9).
  REM-ENCRYPT defines the Sealer role for encrypted copies.
- **Planner**: computes a plaintext object's exact layout and block count
  without payload bytes (Section 4.9).
- **Reader**: recovers entries from the canonical object (Section 4.9).
  REM-ENCRYPT defines how an encrypted copy is opened first.
- **Repacker**: re-emits an object or manifest while preserving its entries (a
  re-pack that does not re-capture from a source filesystem). Its preservation
  obligations are stated in Sections 4.7.5 and 10.
- **Verifier**: validates a complete canonical object end to end (Section
  7.4). REM-ENCRYPT adds keyed and keyless encrypted-copy profiles.
- **Restorer**: maps payload byte ranges to inner ranges (Section 6).
  REM-ENCRYPT maps those ranges through encrypted copies.
- **Scanner**: walks a *plaintext* object using only POSIX tar knowledge —
  the degraded long-term fallback (Section 4.10). Scanners are not required
  to implement this document. No scanner role exists for the encrypted
  representation.
- **Consumer**: interprets a decoded manifest (Section 4.7). A **Restoring
  Consumer** additionally materializes entries to a target filesystem; its
  obligations are stated in Sections 4.7.3 and 12.10.

### 2.3. Definitions

- **Object**: one REM-OBJECT archive; the unit of write, commit, replication, and
  restore.
- **Canonical plaintext object / inner stream**: the complete plaintext
  representation byte string of an object (Section 3.1). REM-ENCRYPT seals
  exactly this byte string.
- **Representation**: one of `plaintext` or `encrypted` (Section 3.2). Two
  stored copies of one object may use different representations.
- **Chunk**: one of the consecutive `chunk_size`-byte units that make up the
  canonical plaintext object (Section 4.2); the unit of alignment and
  addressing within the object.
- **`chunk_size` (C)**: the size in bytes of one chunk. A positive multiple of
  512; default 262144 (256 KiB). One value per object, shared by both
  representations of that object.
- **Record**: a 512-byte tar record. All tar structures are sequences of
  records.
- **Entry**: one pax extended header + one ustar header (typeflag `0`, `1`,
  `2`, or `5`) + record padding, describing one member; only a regular entry
  (`0`) has a payload (Section 4.6).
- **Inner `BodyLba`**: zero-based index of a chunk *within the canonical
  plaintext object*. This is the address space of the manifest and of all
  catalog per-file rows, and it is identical across both representations of
  one object.
- **Stored `BodyLba`**: zero-based index of a stored block within the *stored*
  bytes of one copy. A stored block is a `chunk_size` unit of those bytes; on
  a fixed-block backend it is one backend block. For a plaintext copy, stored
  `BodyLba` equals inner `BodyLba`. REM-ENCRYPT §6.4 defines the encrypted
  mapping.
- **Stored bytes**: the exact bytes of one stored copy, from byte 0 through
  the final byte of its final block. `stored_digest` is defined over these.
- **Deterministic CBOR**: the canonical CBOR encoding rules of Section 4.7.1,
  used by the manifest.

An object's bytes are addressed in three units:

| Unit | Size | Index | Where it applies |
| --- | --- | --- | --- |
| Chunk | `chunk_size` bytes | Inner `BodyLba` | The canonical plaintext object, in both representations |
| Stored block | `chunk_size` bytes | Stored `BodyLba` | The stored bytes of one copy; for a plaintext copy, stored block `n` is chunk `n` |
| Backend block | One stored block | The backend's block address; on tape, the block index within the object's tape file | A fixed-block backend, including tape (Sections 6.4 and 8.2) |

### 2.4. Integer, Byte, and Text Conventions

Byte offsets are zero-based. `KiB` = 2^10 bytes; `MiB` = 2^20 bytes.
Hexadecimal values are prefixed `0x`. ustar numeric fields are ASCII octal
(Section 4.3). All other text in the Core format—pax keywords and values,
paths, and manifest text strings—is UTF-8
[RFC3629]; pax keywords are additionally restricted to ASCII. The functions
`roundup(x, C)` and `roundup512(x)` denote the smallest multiple of `C`
(respectively 512) that is greater than or equal to `x`; when `x` is already
a multiple the result is `x` itself. Equivalently,
`roundup(x, C) = x + ((C − (x mod C)) mod C)`. SHA-256 is the hash
function of [FIPS180-4]. All derived quantities
(offsets, frame lengths, chunk counts, block counts) are defined over unsigned
64-bit arithmetic; implementations MUST use checked arithmetic and MUST NOT
wrap silently (Section 11). PFR denotes partial file restore (Section 6).

### 2.5. Constants

| Constant | Value | Meaning |
| --- | --- | --- |
| `TAR_RECORD_SIZE` | 512 | POSIX tar record size in bytes |
| `DEFAULT_CHUNK_SIZE` | 262144 (256 KiB) | Default chunk size |
| `STREAM_FORMAT_ID` | `rem-object-v1` | Value of the global `REMANENCE.format_id` keyword |
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

## 3. Object Model

### 3.1. The Canonical Plaintext Object

Every REM-OBJECT object has exactly one **canonical plaintext form**: the complete
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
| `encrypted` | The REM-ENCRYPT envelope: an authenticated, confidential wrapper sealing the canonical plaintext object (see REM-ENCRYPT §5) | Confidential and authenticated; self-describing after opening; opaque without a key |

There is no third representation: the plaintext representation is the bare
container stream itself, preserving standard-`tar` extractability, and the
encrypted representation is that same stream sealed. An implementation
producing both copies of one object MUST derive them from the same canonical
plaintext byte string.

*Rationale.* This rule ("build once, fan out") is what makes the shared
identities of Section 3.3 hold.

### 3.3. Identities and Digests

| Digest | Computed over | Stored where | Verifiable without keys? |
| --- | --- | --- | --- |
| `file_sha256` | One regular member file's exact payload bytes | Entry pax header + manifest | Plaintext copies: yes. Encrypted copies: no |
| `manifest_sha256` | The manifest entry's CBOR bytes | Manifest pax header; for plaintext copies also the terminal replicas' Object recovery rows (REM-PARITY §10.3) and the catalog (Section 8.2) | Plaintext copies: yes. Encrypted copies: no |
| `plaintext_digest` | The **complete canonical plaintext object** bytes | Encrypted copies: carried in the REM-ENCRYPT envelope. All copies: catalog | No (for encrypted copies) |
| `stored_digest` | The **complete stored bytes** of one copy, byte 0 through the final fill byte | External only: catalog / master index (never in-band) | **Yes** — the keyless scrub anchor |

Consequences, all normative:

1. For a plaintext copy, `stored_digest` = `plaintext_digest`. The two names
   denote the same value.
2. A plaintext copy and an encrypted copy of the same object share
   `plaintext_digest` and differ in `stored_digest`.
3. `plaintext_digest` is a function of the canonical bytes, which include the
   global header's `object_id` and `write_timestamp` keywords and the final
   zero fill. Copies share it if and only if they wrap the identical
   canonical byte string (Section 3.2). Re-*building* an object from the same input files
   with a new `object_id`, timestamp, or `chunk_size` produces a new object
   with a new `plaintext_digest`; per-file `file_sha256` values are what
   survive across rebuilds.
4. Any stored copy MUST be scrubbable by `stored_digest` alone. The backend
   needs no keys, no plaintext access, and no format knowledge beyond "a byte
   string".

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

For a whole-object input of unknown representation, a Reader first applies
REM-ENCRYPT §3.2. Otherwise it attempts the plaintext representation: the
input must begin with a valid ustar header record, typeflag `g` (Section 4.5),
and its global header must pass the
`REMANENCE.format_id = rem-object-v1` gate (Section 4.5.2). A conformant
plaintext object's first record is the global pax header.

*Rationale.* This rule is for self-identification and tooling convenience.

## 4. Plaintext Representation

The plaintext representation is a constrained subset of POSIX pax tar
[POSIX-PAX], extended with vendor keywords in the `REMANENCE.` namespace and a
generated CBOR manifest stored as the archive's final member. The single
structural constraint beyond plain tar is that every file's payload begins on
a chunk boundary (Section 4.6.3); the unconstrained tar stream remains
extractable by any pax-aware tool, which simply ignores the vendor keywords
(Section 4.10).

### 4.1. Frame Sequence

A plaintext REM-OBJECT object is a byte string with the following layout, where every
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

### 4.2. Chunks and `chunk_size`

The object byte stream is written as consecutive chunks of exactly
`chunk_size` bytes. `chunk_size` MUST be a positive multiple of 512. On tape
`chunk_size` MUST equal the fixed tape block size of the containing tape file
(Section 8.2); one stored block is one tape block. The plaintext
representation defines no maximum. REM-ENCRYPT §5.2 defines the additional
bound for an encrypted copy.

The total object length is always an exact multiple of `chunk_size` (Section
4.8), and the object's block count is knowable before any payload byte is
written (Section 4.8). A Reader is given the object's `chunk_size` and block
count out of band and MUST process exactly that many blocks. The catalog can
supply both. On tape, `chunk_size` is the block size that the bootstrap
records (REM-PARITY §8.1), and the block count is recorded in the object's
Object recovery row (REM-PARITY §10.3) and in its filemark-map entry
(REM-PARITY §7.1). The format reserves no meaning for bytes outside the
object's blocks. It provides no in-band mechanism for locating object
boundaries (Section 4.1, Section 8.2).

### 4.3. The ustar Record Subset

REM-OBJECT emits POSIX ustar headers [POSIX-PAX] restricted as specified here.
Readers MUST validate the checksum of every non-zero header record (Section
4.3.3). The rules of Section 4.3.2 govern the reader-ignored fields.

#### 4.3.1. Header Layout

Every header is one 512-byte record:

| Offset | Length | Field | Value a Builder writes |
| ---: | ---: | --- | --- |
| 0 | 100 | `name` | Entry-dependent (Section 4.3.2); NUL-padded |
| 100 | 8 | `mode` | By entry type (table below) |
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
| 345 | 155 | `prefix` | All NUL (Builders never use `prefix`) |
| 500 | 12 | — | All NUL |

The `mode` values by entry type:

| Entry | `mode` |
| --- | --- |
| Regular file | `0000644\0`, or `0000755\0` when `REMANENCE.executable` is `true` |
| Hardlink | `0000644\0` |
| Symbolic link | `0000777\0` |
| Directory | `0000755\0` |
| Pax header record (`g`, `x`) | `0000644\0` |

Octal fields are zero-padded ASCII octal terminated by NUL. When parsing,
Readers MUST stop a numeric field at the first NUL or space, MUST accept
surrounding ASCII whitespace, and MUST treat an empty field as zero. The
`uid`, `gid`, ustar `mtime`, `uname`, and `gname` fields carry the fixed
values above regardless of metadata-preservation tier. Readers MUST ignore
these fields.

*Rationale.* This specification deliberately does not preserve ownership, so a
root-run standard `tar` extraction cannot apply ownership the format never
recorded.

#### 4.3.2. Reader-Ignored Fields; Names and Sizes

Readers MUST NOT base acceptance on `mode`, `uid`, `gid`, ustar `mtime`,
`uname`, `gname`, `devmajor`, `devminor`, or `version`. Readers MUST honor
`prefix` when forming a header path from a foreign ustar header
(`prefix + "/" + name` when `prefix` is non-empty). Conformant Builders leave
`prefix` empty.

The authoritative path and size of an entry are its pax `path` and `size`
records (Section 4.4.4). The ustar header nevertheless remains well-formed:

- **Name.** If the effective path is non-empty, at most 100 bytes, and
  consists solely of non-control ASCII, the Builder MUST store it in `name`
  verbatim; otherwise the Builder MUST store `PAX_PATH_PLACEHOLDER`
  (`remanence/pax-path`).
- **Size.** If the payload length is ≤ `USTAR_SIZE_MAX`, the Builder MUST store
  it in `size`; otherwise the Builder MUST store zero. The pax `size` record is
  always present and authoritative, so files ≥ 8 GiB are fully supported.
- The ustar names of the pax header records themselves are the fixed
  constants `GLOBAL_HEADER_NAME`, `PAX_HEADER_NAME`, and
  `MANIFEST_PAX_HEADER_NAME` (Section 2.5). They exist for human-readable
  `tar -t` listings; Readers MUST NOT interpret them.

#### 4.3.3. Checksum

The `chksum` field holds the unsigned sum of all 512 header bytes with the
eight checksum bytes treated as ASCII spaces (0x20), encoded as six ASCII
octal digits, a NUL, and a space. Builders MUST emit exactly this encoding.
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
| NUL (0x00) | Accepted by Readers as a regular file (pre-POSIX compatibility); Builders MUST NOT emit it |

Readers MUST reject any other typeflag with `UnsupportedTarTypeflag`.

*Rationale.* The Core entry set is a faithful tree of files — regular files,
hardlinks, symbolic links, and directories — with no device, FIFO, socket, or
other special entry (Section 1.5). Accepting an unsupported typeflag silently
would misrepresent an unsupported archive as fully restored.

### 4.4. Pax Extended Records

#### 4.4.1. Record Grammar

A pax header's payload is a sequence of records, each:

```text
"<len> <keyword>=<value>\n"
```

where `<len>` is the decimal byte length of the entire record including the
length digits themselves, the single space, and the trailing newline. `<len>`
is self-referential. A Builder MUST compute it by fixed-point iteration over
its own digit count, starting from `base`, the record length with zero length
digits:

1. Initialize `digits` to `digits(base)`, the number of decimal digits in `base`.
2. Set `len` to `base` plus `digits`.
3. If `len` has exactly `digits` decimal digits, stop: `len` is the value.
4. Otherwise set `digits` to the number of decimal digits in `len`.
5. Continue at step 2.

This is the iteration `len ← base + digits(len)`, run until stable. For a base
of 20, `digits(base)` is 2 and `len` is 22, which has two digits, so `<len>`
is 22. For a base of 8, `digits(base)` is 1 and `len` is 9, which has one
digit, so `<len>` is 9.

At certain base lengths (8, 97, 996, 9995, …) two self-consistent values of
`<len>` exist: a base of 8 is also consistent with 10. These are the bases at
which adding the digit count lands one short of a power of ten. The upward
iteration converges to the smaller value, and a Builder MUST emit that smaller
value. Section 13.2 gives the pad records that the rule produces for
REM-OBJECT-TV-P1.

Constraints, enforced by both Builders and Readers:

- `<keyword>` MUST be non-empty ASCII and MUST NOT contain `=`, newline, or
  NUL.
- `<value>` MUST be valid UTF-8 and MUST NOT contain any byte < 0x20. A pax
  value in this format is therefore always single-line.
- `<len>` MUST be ≥ 1 and MUST NOT exceed the remaining header payload.
- The record MUST end with exactly one newline at offset `<len> − 1`.

Readers MUST reject violations with `PaxRecordMalformed`.

#### 4.4.2. Emission Order and Duplicates

Builders MUST emit the records of one pax header sorted in ascending bytewise
order of the keyword, and MUST NOT emit the same keyword twice in one header.
As a consequence, all `REMANENCE.*` keywords sort before the lowercase
standard keywords `mtime`, `path`, `size`. Readers MUST apply POSIX last-wins
semantics if duplicates are encountered in a foreign archive. Readers MUST NOT
reject an archive solely for unsorted records. Determinism is an obligation on
the Builder, not a read-acceptance rule.

#### 4.4.3. Unknown Keywords

Readers MUST ignore unknown keywords, including unknown `REMANENCE.*`
keywords. Unknown keywords are the format's extension mechanism: a future
revision of this document may assign new keywords (Section 10). Readers SHOULD
preserve unknown keywords when re-emitting metadata. A Reader MUST NOT let an
unknown keyword alter payload framing or interpretation.

#### 4.4.4. Standard Keywords Used

| Keyword | Presence | Meaning |
| --- | --- | --- |
| `path` | REQUIRED on every entry | Effective UTF-8 entry path; overrides the ustar `name` |
| `size` | REQUIRED on every entry | Effective payload byte length (decimal); overrides the ustar `size` |
| `linkpath` | Symlink/hardlink entries when needed | Effective symlink target (an opaque string), or hardlink target (an in-object path, Section 4.6); overrides the ustar `linkname` |
| `mtime` | OPTIONAL | Modification time in POSIX pax decimal form: non-negative decimal seconds since the epoch, optionally followed by `.` and fractional digits. The value is a caller-supplied string. Builders MUST validate this shape and MUST emit the validated string verbatim. The byte stream is then a deterministic function of the caller's input |

Builders MUST always emit `path` and `size`, even when the ustar header could
carry them. Every entry is then self-describing under pax rules alone. Readers
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
| `REMANENCE.format_id` | MUST be `rem-object-v1` |
| `REMANENCE.metadata_preservation` | One of `minimal`, `archival`, `full` |
| `REMANENCE.object_id` | Object identifier of 1–64 non-NUL UTF-8 bytes (a UUID string in practice; opaque to this format). The bound is intrinsic because the representation-independent REM-PARITY Object recovery row (REM-PARITY §10.3, key 4) carries the identifier verbatim. |
| `REMANENCE.schema_version` | `<major>.<minor>` decimal text; MUST have major version 1 (Section 10) |
| `REMANENCE.write_timestamp` | [RFC3339] timestamp of object creation |

#### 4.5.2. Validation

Before delivering any entry, a Reader MUST verify on the accumulated global
records:

1. `REMANENCE.format_id` is present and equals `rem-object-v1` (`UnsupportedFeature`
   otherwise; a missing key is `Parse`).
2. `REMANENCE.schema_version` is present and its major component (the decimal
   text before the first `.`, or the whole value if no `.`) parses as an
   unsigned integer equal to 1 (`UnsupportedFeature` on mismatch, `Parse` on
   malformed).
3. If `REMANENCE.encryption` is present, it equals `none`
   (`UnsupportedFeature` otherwise).

   *Rationale.* This is a refusal gate: a Reader that ignored it could restore
   ciphertext as content under a future revision. Confidentiality is provided
   exclusively by the REM-ENCRYPT envelope around the stream, never flagged
   inside it.
4. If `REMANENCE.chunk_size` is present, it equals the externally supplied
   `chunk_size` (`ChunkSizeMismatch` otherwise).

   *Rationale.* A mismatch means the object is mis-cataloged or the tape was
   rewritten with different geometry, and restoring under the wrong geometry
   mis-addresses every chunk.

The remaining global keywords (`object_id`, `caller_object_id`,
`metadata_preservation`, `write_timestamp`) are descriptive; Readers MUST NOT
require them for acceptance. Consumers cross-check the identity keywords
against the manifest (Section 4.7). A conformant Builder emits exactly one
global header, first. Readers MUST accept a foreign archive that contains
additional `g` headers later in the stream. They MUST merge those headers'
records with last-wins semantics and re-run these checks before the next entry
is delivered.

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

For symbolic links, Builders MUST store the target in ustar `linkname` when it
fits in 100 bytes; otherwise they MUST store it in pax `linkpath` and store
`PAX_LINK_PLACEHOLDER` (`remanence/pax-linkpath`) in `linkname`. A symlink target is an opaque UTF-8 OS string, not
a REM-OBJECT path: it MAY be absolute, contain `..`, or be dangling. Directory
paths MUST end in `/`.

**Hardlinks.** A hardlink entry records that its path is a second name for the
bytes of another entry — the **primary** — in the same object. Its target is
stored exactly as a symlink target is: in `linkname`, or in pax `linkpath`
with `PAX_LINK_PLACEHOLDER` in `linkname`. Unlike a symlink target, it is
**not** an arbitrary string. It MUST be a canonical relative path (Section
4.6.6) that resolves, within the same object, to a **regular-file primary
entry appearing before** the hardlink entry. Of a set of names sharing one
underlying file the **primary** is one regular entry that holds the bytes;
each other name is a hardlink entry. Primary selection MUST be deterministic
and is defined over the entries the object emits: the primary is the first, in
archive order, of the set's names that the object emits as entries. If the
object emits only one name of the set, that name is a plain regular entry (no
hardlink entry).

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
`REMANENCE.file_sha256` is not consulted during framing; its
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
the pad record, and `R = roundup512(B)`. The Builder MUST choose the pad length
such that

```text
O + 512 + R + 512 ≡ 0   (mod chunk_size)
```

The two 512s are the pax ustar record and the file ustar record. Because the
byte stream must be deterministic, the pad length is uniquely determined by
these steps:

1. Let `Rmin` be `roundup512` of the pax payload including an empty-valued pad
   record.
2. Let the target `R` be the smallest multiple of 512 that is ≥ `Rmin` and
   satisfies the congruence.
3. Let the pad value be the largest number of spaces for which the pax
   payload length does not exceed `R`, solving each candidate record's
   self-referential length per Section 4.4.1.
4. If that payload rounds up to exactly `R`, the pad length is found.
5. Otherwise, at a decimal-digit-boundary corner, advance `R` by `chunk_size`
   and continue at step 3.

A Builder MUST fail with `Layout` rather than emit a misaligned entry if no
solution exists within `4 × chunk_size` above `Rmin`. Appendix A gives the
resulting pad values for REM-OBJECT-TV-P1.

*Rationale.* The pad is never a standalone tar member. It is a legitimate pax
record of the entry it aligns, invisible to standard tools.

There is no padding *after* payloads beyond tar's normal 512-byte record
padding: a chunk may contain one file's tail bytes followed immediately by the
next entry's headers. Readers recover exact sizes from `size`, never from
chunk boundaries.

#### 4.6.4. Chunk Geometry

For an entry with effective size `Z` and payload offset `D`:

```text
chunk_count     = 0                       if Z = 0
                  ceil(Z / chunk_size)    otherwise
first_chunk_lba = absent                  if Z = 0
                  D / chunk_size          otherwise
```

`first_chunk_lba` is an inner `BodyLba`. Byte range `[s, s+n)` of the file
maps to chunks
`first_chunk_lba + floor(s / chunk_size) ..= first_chunk_lba + floor((s+n−1) / chunk_size)`
with head/tail trimming; the final chunk holds
`Z − (chunk_count−1) × chunk_size` payload bytes (plus whatever follows in the
stream). Range requests MUST be validated against `Z` with checked arithmetic
before mapping (Section 6.1).

#### 4.6.5. Payload Hashing

For regular entries, `REMANENCE.file_sha256` is computed over the exact `Z`
payload bytes — never over tar headers, padding, or the final zero fill. When
the Builder receives payload bytes as a stream, it MUST recompute the SHA-256
of the bytes actually consumed. If the recomputed digest or byte count differs
from the declared spec, the Builder MUST fail the object and refuse to
complete it. Symlink, directory, and hardlink entries carry no payload and no
payload hash of their own. A hardlinked name's content, hash, and PFR
coordinates are its primary's, reached through `link_target` (Section 4.7.2).

*Rationale.* The recomputation proves that the Builder archived the payload it
was given, not the payload the metadata describes.

#### 4.6.6. Path and Identity Rules

For every member entry, Builders MUST enforce:

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
(`InvalidPath`). A traversal-shaped or non-canonical path is nonconformant.
Rules 3–5 bind the Builder; Verifiers and Consumers catch duplicates via the
manifest (Section 4.7). Within those rules, paths are byte sequences stored
verbatim: the format performs no Unicode normalization (NFC and NFD spellings
of the same name are distinct paths), no case folding, and no separator
translation.

*Rationale.* Accepting a traversal-shaped or non-canonical path would push the
hazard onto every downstream consumer (Section 12.10).

Symlink targets are not entry paths and MUST NOT be validated with the
canonical-relative-path rule. A target is a pax value: valid UTF-8, no NUL,
and no byte < 0x20. It may be absolute, contain `..`, or point to a missing
target; Section 12.10 describes the hazard this creates on restore.

#### 4.6.7. Entry Order

Payload entries appear in caller-supplied order; the format assigns no meaning
to the order beyond determinism. The manifest MUST be the last entry. Readers
identify the manifest by its exact path `_remanence/manifest.cbor` and
MUST NOT rely on `REMANENCE.is_manifest` alone. Readers MUST reject any entry
appearing after the manifest entry (`Parse`).

*Rationale.* Such an entry cannot be listed in the manifest, so the object's
self-description would be silently incomplete.

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
manifest entry itself. Its own identity lives in its pax header and,
externally, in the Object recovery row (REM-PARITY §10.3; plaintext copies,
Section 8.2), which enables direct LOCATE-to-manifest reading without scanning
the archive.

#### 4.7.1. Deterministic CBOR

A manifest is a single CBOR [RFC8949] data item in the **manifest profile** of
REM-OBJECT's deterministic CBOR. **Item repertoire** — each item MUST be one of:

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
appear. A decoder MUST reject them with `Cbor`.

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
decode-and-re-encode.

**Structural limits.** An object MUST NOT contain more than `MAX_FILE_ENTRIES`
(10,000,000) member entries. Manifest nesting depth MUST NOT exceed
`MANIFEST_MAX_DEPTH` (8), counting the top-level map as depth 1. Decoders MUST
enforce both limits.

#### 4.7.2. Schema

The top-level item has the following seven required text keys (shown in
encoded sort order). A 1.0 Builder emits exactly these seven top-level keys; a
Reader requires all seven and additionally tolerates unknown *bare* keys as
reserved for future revisions under Consumer obligation 3 below.

| Key | Type | Constraint |
| --- | --- | --- |
| `object_id` | text | MUST equal the global `REMANENCE.object_id` |
| `chunk_size` | unsigned | MUST equal the global `REMANENCE.chunk_size` |
| `file_entries` | array | One file-entry map per member entry (regular/hardlink/symlink/directory), in archive order |
| `schema_version` | unsigned | MUST be 1 (`MANIFEST_SCHEMA_VERSION`) |
| `object_metadata` | map | Empty (`{}`), or the inventory of Section 4.7.6, optionally with an `ext` container (Section 4.7.5) |
| `caller_object_id` | text | MUST equal the global `REMANENCE.caller_object_id` |
| `external_references` | array | Reserved; a 1.0 Builder MUST leave it empty (`[]`) |

Each `file_entries` element is a map with the base keys below, plus the
conditional non-regular keys. The table groups the keys by function; on the
wire they appear in the deterministic order of Section 4.7.1. Regular entries
MUST NOT carry `entry_type` or `link_target`.

*Rationale.* Omitting the two keys preserves the pre-expansion byte
representation for regular-only objects.

| Key | Type | Constraint |
| --- | --- | --- |
| `path` | text | Effective entry path |
| `file_id` | text | Entry `REMANENCE.file_id` |
| `executable` | `true`/`false`/`null` | `null` when the Builder was given no value |
| `size_bytes` | unsigned | Effective payload length (0 for hardlink/symlink/directory/empty entries) |
| `chunk_count` | unsigned | Section 4.6.4 value (0 for zero-payload entries, hardlinks included) |
| `entry_type` | text | OPTIONAL; absent means `regular`; otherwise `hardlink`, `symlink`, or `directory` |
| `file_sha256` | bytes | Regular entries only: exactly 32 bytes; binary SHA-256 (hex in pax, binary here). A hardlinked name's hash is its primary's, reached via `link_target` |
| `first_chunk_lba` | unsigned/`null` | Inner `BodyLba`; `null` if and only if `size_bytes` = 0 (so `null` for hardlinks) |
| `link_target` | text | Symlink entries: the effective target string. Hardlink entries: the primary's in-object path (Section 4.6) |
| `metadata_preservation_data` | map | Empty, the xattr container of Section 4.7.3, an `ext` container (Section 4.7.5), or both; hardlink entries MUST use an empty map |

Consumer obligations:

1. A Consumer MUST NOT rely on a manifest field unless it has verified the
   manifest bytes against an anchor digest. A failure is
   `ManifestDigestMismatch`. An anchor digest is the `manifest_sha256` of an
   Object recovery row or of the catalog, when one is available. For
   self-consistency only, the manifest entry's own pax
   `REMANENCE.file_sha256` is an anchor digest. An unverified manifest is
   untrusted input from removable media. For encrypted copies this
   obligation is discharged by the envelope's authenticated whole-object
   digest as specified by REM-ENCRYPT §7.1; external manifest anchors are
   required for plaintext copies only.
2. A Consumer MUST reject a manifest violating the type or value constraints
   above (`ManifestInvalid`), including the cross-checks: `object_id`,
   `caller_object_id`, and `chunk_size` MUST equal the corresponding global
   header values when both are in hand, and no two `file_entries` elements
   may share a `path` or a `file_id`.
3. A Consumer MUST treat unknown bare keys as **reserved for future revisions
   of this document** and ignore them. This covers top-level and per-entry
   keys, including unknown bare keys within `metadata_preservation_data` and
   `object_metadata`. Third-party data lives only under `ext` (Section 4.7.5),
   never in a bare key. A Consumer MUST NOT reject a manifest merely because a
   reserved map or array is non-empty.

**Verifier obligation.** A Verifier MUST verify that the manifest and the
archive entries correspond exactly (Section 7.4). They correspond exactly when
they have the same paths, entry types, link targets, sizes, hashes where
present, and chunk geometry, with no extras on either side.

#### 4.7.3. Extended-Attribute Preservation

An entry's `metadata_preservation_data` MAY contain the following map entry:

```text
"xattrs" : { <name> : <value>, ... }
```

`<name>` is a nonempty CBOR text string containing the attribute name. It MUST
be valid UTF-8 [RFC3629] and MUST NOT contain an ASCII control byte below
`0x20`. No escaping is defined. A Builder MUST reject a name violating these
rules.
The **namespace** of an attribute name is the substring preceding its first
`.`; a name containing no `.` has no namespace. This document's validity rule
for names is unchanged (nonempty UTF-8, no ASCII control byte below `0x20`);
the namespace derivation is a classification rule, not an acceptance rule, and
does not shrink the set of valid names.

The name is stored in **canonical wire form** as `namespace.name`; producing
that form is an obligation on the Builder. A platform's native attribute model
can differ from it: the namespace can be a separate argument, the namespace
can be flat, or the store can fold case. A Builder on such a platform MUST map
its native namespace to the canonical prefix deterministically and MUST NOT
remap one namespace onto another. For example, a `user.` namespace attribute
is `user.name`. A native attribute that a Builder cannot represent with a
derivable namespace is not captured; such attributes include a name with no
`.` and a name that a case-folding store cannot round-trip without altering
case. The canonical name bytes are identical across independent Builders for
the same native attribute on the same platform. Whole-manifest byte identity
additionally requires identical object parameters (Section 1.2 goal 6).

`<value>` is a CBOR byte string containing the raw attribute value without a
textual encoding. The `xattrs` map follows the deterministic encoding rules of
Section 4.7.1, including encoded-key ordering and the prohibition on duplicate
names. Readers MUST ignore unknown keys in `metadata_preservation_data`. Those
keys are reserved for future revisions; third-party data lives under `ext`
(Section 4.7.5).

An entry with no preserved xattrs MUST NOT carry an `xattrs` key. An entry
with neither preserved xattrs nor an extension container (Section 4.7.5) MUST
carry an empty `metadata_preservation_data` map. A hardlink entry MUST carry
an empty map. The shared file's restored xattrs come from the regular-file
primary named by `link_target`. Ownership, ACLs as a separate REM-OBJECT
semantic, and mode bits beyond `executable` remain outside this format.
`mtime` is already represented by the pax `mtime` keyword.

A Builder that emits no preserved xattrs anywhere MUST set
`REMANENCE.schema_version = 1.0`. A Builder that emits at least one preserved
xattr MUST set it to `1.1`. In both cases the manifest CBOR `schema_version`
integer remains 1. This gate is independent of REM-ENCRYPT versioning
(Section 10).

Which xattrs an ingesting system selects is policy outside this byte format.
This document defines two tiers, and both are carried faithfully. The
**portable core** is the `user.` attribute namespace. The **extension tier**
is every attribute that is not in the `user.` namespace, including an
attribute whose name has no namespace, and every extension (Section 4.7.5).
An item that a Consumer carries but does not act on is **carry-only**.

A Restoring Consumer MUST surface any attribute application failure rather
than silently declaring success.

#### 4.7.4. Within-Stream Chain of Trust

```text
Object-recovery-row/catalog anchor (manifest location + manifest_sha256; plaintext copies — Section 8.2)
        │  externally anchored
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
chain provides integrity, not authentication (Section 12.6); the anchor for
encrypted copies is defined by REM-ENCRYPT §7.1.

#### 4.7.5. Extension Containers

An entry's `metadata_preservation_data` map and the object-level
`object_metadata` map (Section 4.7.2) MAY carry a single reserved indirection
key, `ext`, whose value is a map. A hardlink entry's
`metadata_preservation_data` MUST remain empty (Section 4.7.3) and MUST NOT
carry `ext`. A non-map `ext` value makes the manifest nonconformant
(`ManifestInvalid`). Every bare (non-`ext`) key in these two maps is reserved
to this specification and its successors. Third-party and platform-specific
data MUST live only under `ext`.

Each member of an `ext` map is one extension, keyed by an **extension name**.
An extension name takes one of two forms, and the presence of a `.`
distinguishes them:

- A **reverse-DNS name** is lowercase, contains at least one `.`, and lies in
  a domain the author controls, for example `org.example.thing`. It requires
  no registration.
- A **registered short name** is lowercase and hyphen-separated, contains no
  `.`, and comes from the community list (Section 15).

A malformed or uppercase extension name is treated as unrecognized: it is
ignored and carry-only, not a reason to reject the object. An `ext` member
value MUST use the manifest CBOR profile of Section 4.7.1, with
definite-length items and the permitted major types only. It counts against
the Section 4.7.1 depth limit. A non-conforming `ext` value makes the whole
manifest nonconformant (`Cbor`).

Extension processing is fail-safe, carry-only, and additive:

- A Consumer **recognizes** an extension only if it implements that
  extension's semantics; knowing an extension's name or any registered
  disposition is not recognition.
- A Consumer MUST ignore an `ext` member it does not recognize and MUST NOT
  reject an object for its presence.
- A Repacker (Section 2.2) MUST reproduce unchanged the canonical CBOR
  encoding of every `ext` member it does not recognize. Preserving the decoded
  value is equivalent, because under Section 4.7.1 the canonical re-encoding
  is identical. Silently dropping an unrecognized extension is nonconformant.
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
namespace or any `ext` member, the object's `object_metadata` map MUST carry
an inventory. The inventory lets a holder determine what non-core metadata the
object contains without decoding `file_entries`. It is a map with exactly
these keys:

| Key | Type | Value |
| --- | --- | --- |
| `attribute_namespaces` | array of text | the set of distinct non-`user.` attribute namespaces (Section 4.7.3) present across all entries, sorted in the order Section 4.7.1 defines for text map keys (encoded length prefix, then key bytes) |
| `extensions` | array of text | the set of distinct `ext` extension names present across all entries and in `object_metadata`, sorted in the order Section 4.7.1 defines for text map keys (encoded length prefix, then key bytes) |

Both arrays carry names only; attribute values and per-entry detail MUST NOT
appear. An empty array is omitted (its key absent). For verification, an
absent inventory key is treated as an empty array; a present empty array is
accepted (the Builder's determinism is not a read-acceptance rule, Section
4.4.2). An object carrying only the portable core and no `ext` MUST leave
`object_metadata` empty (`{}`). A Consumer MUST treat an unrecognized
`object_metadata` key as reserved-for-future-use and ignore it (Section 4.7.2
obligation 3).

**Verifier obligation.** A Verifier (Section 7.4) MUST confirm that the
inventory is exact and MUST reject a mismatch (`ManifestInvalid`). The
inventory is exact when `attribute_namespaces` equals the set of non-`user.`
namespaces actually present, and `extensions` equals the set of `ext` names
actually present across all entries and in `object_metadata`.

A holder MAY rely on the inventory as a disclosure-screening surface only for
an object that has passed Verifier validation.

### 4.8. End of Archive

After the manifest entry's padding, Builders MUST emit exactly two all-zero
512-byte records. Readers MUST treat an all-zero header record followed by a
second all-zero record as end of archive. Readers MUST reject an all-zero
record followed by a non-zero record (a single zero tar EOF record) with
`Parse`.

After the EOF records, Builders MUST fill the remainder of the final chunk
with zero bytes. The object's total length is then:

```text
total_size_bytes      = roundup(offset_after_EOF, chunk_size)
projected_size_blocks = total_size_bytes / chunk_size
```

This is the only chunk-level zero fill in the stream, and it is tar-safe: it
lies beyond the archive EOF where standard tar already stops. Readers MUST NOT
interpret bytes after the EOF records as archive members. A Reader MAY stop at
the EOF records or MAY validate the remaining fill and reject a nonzero byte
as a nonconformity. Verifiers (Section 7.4) MUST confirm the fill is all-zero
and report a nonzero fill as a nonconformity. A Builder whose emitted block
count differs from its planned `projected_size_blocks` MUST fail the object
rather than complete it.

### 4.9. Builder, Planner, and Reader Obligations

**Builder and Planner.** The Planner computes the entire layout from the file
*specs* alone, without payload bytes. The specs are the path, file_id, entry
type, link target where present, size, hash, optional mtime/executable, and
1.x preservation metadata. The layout is every offset, pad size, manifest
byte, and the final block count. The Planner and the Builder MUST share the
sizing rules that make the planned layout byte-exact. A failed object MUST NOT
be reported as complete. A Builder MUST fail the object when a block write
commits fewer bytes than the full block or reports hard end-of-medium. The
failure is `IncompleteBlockWrite`. Recommended practice for a Builder's
pipeline is described in the REM Implementation and Operations Guide, under
“Staging, commit and durability”.

**Reader.** A Reader receives a block source positioned at the object's inner
`BodyLba(0)`, the object's `chunk_size`, and its block count. A Reader
operates in one of two modes. **Restore** mode verifies integrity.
**Salvage** mode, for damaged media, reports verification failures but
continues delivery. A Reader MUST NOT silently fall back to salvage.
A Reader processes the object as follows:

1. Read 512-byte records. A short block read is a hard error.
2. On an all-zero record: require the second EOF record (Section 4.8), run the
   Section 4.5.2 global checks (covers empty objects), and stop. Remaining
   blocks are ignored.
3. Verify the header checksum (Section 4.3.3).
4. Dispatch on typeflag:

   | Typeflag | Reader action | Error |
   | --- | --- | --- |
   | `g` | Merge the records into the global set (last-wins), and defer re-validation to the next entry. | |
   | `x` | Parse the records and attach them to the next entry. | |
   | `0`/NUL | Treat the entry as a regular entry. Run the global checks if they have not yet run for the current global set. Compute the effective path and size, verify `REMANENCE.compression`, and verify chunk alignment if `size > 0`. Deliver exactly `size` payload bytes, then skip the record padding. | `TruncatedPayload` if EOF falls inside a declared payload or its padding |
   | `1` | Treat the entry as a hardlink. Require `size = 0`, and compute the effective path and the in-object target (`linkpath` or `linkname`). Verify that the target resolves to a regular-file primary already delivered. Deliver a hardlink entry with no payload; its content and PFR resolve through `link_target` to that primary. | `InvalidHardlinkTarget` if the target is not a regular-file primary already delivered |
   | `2` | Treat the entry as a symlink. Require `size = 0`, compute the effective path and target (`linkpath` or `linkname`), and deliver a symlink entry with no payload. | |
   | `5` | Treat the entry as a directory. Require `size = 0`, and deliver a directory entry with no payload. | |
   | Any other | Reject the typeflag (Section 4.3.4). | `UnsupportedTarTypeflag` |

5. **Integrity (restore mode).** For every regular entry delivered in full,
   compute SHA-256 over the delivered payload bytes while streaming and
   compare it against `REMANENCE.file_sha256`. On a mismatch, fail the entry
   with `FileDigestMismatch` before reporting it restored. In salvage mode,
   deliver the entry but report the mismatch. Hardlink, symlink, and directory
   entries have no payload hash of their own. They are verified through the
   manifest and object digest chain, and a hardlink also through its
   referential integrity (Section 4.6). Section 6.5 states what integrity a
   range read can claim.
6. Capture the entry whose effective path is `_remanence/manifest.cbor` as the
   manifest bytes. An object whose EOF is reached with no manifest entry is
   nonconformant. A Verifier MUST reject it (Section 7.4). A restore-mode
   Reader SHOULD report the absence to its caller.

A conformant Reader accepts a mildly foreign archive where that is safe:
unsorted or duplicate pax records, the NUL typeflag, `prefix`-formed names,
missing pax `path` or `size` with ustar fallback, and later `g` headers. It
rejects the cases in which silent acceptance would misrepresent the object:
unknown typeflags, an unknown format or major version, non-`none` compression,
misaligned data, traversal-shaped paths, and checksum mismatches.

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
claim (Section 12.10). With all REM-OBJECT-specific metadata lost, a Scanner can still
walk the archive using only tar rules — header, `size`, `roundup512(size)`,
repeat — recovering payload bytes, hardlink relationships, symlink targets,
directory entries, and names; it loses only chunk addressing (irrelevant when scanning) and
verification (recoverable from the manifest if its blocks survive).
Conformance requires demonstrated extraction equality by GNU tar, bsdtar, and
Python `tarfile` (Section 14).

## 5. Encrypted Representation (in REM-ENCRYPT)

## 6. Partial File Restore

PFR maps a member-file byte range to stored byte ranges by closed-form
arithmetic. The per-file index — `first_chunk_lba` (an inner `BodyLba`) and
`size_bytes` per file, from the manifest or the catalog — is the **same for
both representations** of an object, because both wrap the same canonical
bytes. Stored offsets are reproducible from this section (plaintext) and
REM-ENCRYPT §6.3 (ciphertext).

**Hardlinks.** A hardlink entry has `size_bytes = 0` and `first_chunk_lba`
`null` (Section 4.7.2); it stores none of its own content. For PFR on a
hardlinked name, a Restorer MUST first resolve its `link_target` to the
primary entry and then use the **primary's** `first_chunk_lba` and
`size_bytes` for all arithmetic below. A Restorer MUST NOT treat a hardlinked
name as an empty or invalid range. Symlinks and directories carry no payload
and are not PFR targets.

Recommended practice for catalogs and per-file indexes is described in the
REM Implementation and Operations Guide, under “Catalogs and indexes”.

### 6.1. Range Validation

Given a file with size `Z` and a requested range `[s, s + n)`: if `n = 0`
the result is the empty range set, provided `s` is itself within the file
(`s ≤ Z`); a zero-length request at an out-of-range offset is rejected like
any other out-of-range request. Otherwise the Restorer MUST validate
`s + n ≤ Z` with checked arithmetic before applying any formula below. The
formulas are defined only for validated, non-empty ranges.

### 6.2. Inner Mapping (Both Representations)

With `C = chunk_size`, `L = first_chunk_lba`:

```text
b_first = L + floor(s / C)
b_last  = L + floor((s + n − 1) / C)
```

File byte `x` lives in chunk `L + floor(x / C)` at offset `x mod C` (file
payloads start chunk-aligned, Section 4.6.3). The requested bytes are obtained
from chunks `b_first ..= b_last` with head/tail trimming. The final chunk of a
file holds `Z − (chunk_count − 1) × C` payload bytes, with unrelated stream
bytes after them (Section 4.6.4). Trim by `Z`, never by chunk boundaries. For
a **plaintext copy** this is the whole computation: its chunks are its stored
blocks, so read them and trim.

### 6.3. Ciphertext Mapping (in REM-ENCRYPT)

### 6.4. Stored-Block Mapping (Tape and Block-Addressed Backends)

On a byte-addressed backend, a stored byte range is fetched directly. On a
fixed-block backend, including the tape binding of Section 8.2, a non-empty
stored byte range `[a, a + l)` maps to:

```text
first_stored_block = floor(a / C)
last_stored_block  = floor((a + l − 1) / C)
```

### 6.5. Integrity of a Range Read

A range read that does not cover the whole file cannot verify a whole-file
`file_sha256`. The integrity of such a read depends on the representation and
the backend, in one of three cases:

- (a) A **parity-protected tape** plaintext copy is covered by the parity
  layer's per-block CRCs ([REMPARITY]). They detect damage but do not
  authenticate: CRC-64 is unkeyed, so whoever substitutes a block can also
  supply its matching CRC.
- (b) An **encrypted** copy follows the authenticated range-read rules of
  REM-ENCRYPT §6.3.
- (c) A **plaintext copy on a byte-addressed backend without the parity
  layer**, such as a file or object store, has **no per-range integrity by
  construction**. A verifying range read there requires either the encrypted
  representation or a whole-file `file_sha256`/`plaintext_digest` check, which
  reads the whole file.

A Restorer MUST report which of the three cases applies to such a read, so
that the report does not imply hash-verified content. An implementation
MUST NOT present a read of case (c) as integrity-verified.

## 7. Digests, Integrity, and the Verification Chain

### 7.1. The Chain of Trust

```text
external catalog / plaintext Object recovery row anchor
(stored_digest, plaintext_digest, manifest location + manifest_sha256)
        │
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
within-stream cross-check. This plaintext chain provides integrity, not
authentication (Section 12.6). Encrypted copies add an authenticated envelope
layer (REM-ENCRYPT §7.1).

### 7.2. Write-Path Verification (No Extra Reads)

Every digest in the chain is computed over bytes already flowing through the
Builder — the chain costs hash arithmetic, never an additional read pass.

**Per-file, at build.** The Builder streams each payload file, hashing it, and
MUST fail the object if the streamed SHA-256 or byte count differs from the
caller-supplied expected `file_sha256`/size (Sections 4.6.5, 4.9). A failed
object MUST NOT be completed or reported as complete.

*Rationale.* This check proves that the Builder archived the payload it was
given, not the payload the metadata describes.

REM-ENCRYPT §7.2 specifies the additional encrypted write-path discharge.

### 7.3. Post-Write Re-Verification

Recommended practice for re-verifying a copy after it is written is described
in the REM Implementation and Operations Guide, under “Staging, commit and
durability”.

### 7.4. Verifier Profile

A Core Verifier performs these checks:

- the full restore-mode read of Section 4.9, with every regular entry's
  digest checked;
- manifest anchor-digest and schema validation (Section 4.7.2);
- manifest-vs-archive correspondence, as defined below;
- exact `object_metadata` inventory validation (Section 4.7.6);
- the final-fill zero check (Section 4.8); and
- a `stored_digest` comparison against the catalog value, when one is
  available.

The manifest and the archive correspond when every member entry appears in
`file_entries` with matching `path` and `entry_type`, and `file_entries` lists
nothing absent from the archive. In addition:

- **regular entries** match `size_bytes`, `file_sha256`, `first_chunk_lba`,
  and `chunk_count`;
- **hardlink entries** match `link_target`, carry zero/`null` content fields
  and no `file_sha256`, and resolve to a valid regular-file primary
  (Section 4.6); and
- **symlink/directory entries** carry zero/`null` content fields.

REM-ENCRYPT §7.4 defines the keyed and keyless encrypted-copy profiles and
requires the recovered inner stream to pass this Core profile.

### 7.5. Scrub

Recommended practice for scrub is described in the REM Implementation and
Operations Guide, under “Verification, scrub and repair”.

## 8. Storage Bindings and Backend Independence

### 8.1. The Byte-Format Contract

A REM-OBJECT in either representation is a byte string. Any conformant tool
can produce it; any backend can store it; `stored_digest` is computed over the
identical bytes everywhere ("byte-stable fanout"). A backend needs no keys, no
plaintext access, and no format knowledge to store, replicate, compare, or
scrub a copy.

### 8.2. Tape Binding

The stored bytes are written as one tape file of fixed-size tape blocks,
terminated by a filemark written by the parity layer at object close. The tape
block size MUST equal the object's `chunk_size`, for both representations. One
stored block is therefore one tape block, stored `BodyLba` is the tape file's
block index, and parity geometry is uniform. Parity sidecars, the filemark
map, block CRCs, and the BOT bootstrap are tape-binding artifacts owned by the
parity layer (Section 9); they exist **only** on tape and are not part of the
object's stored bytes on any backend.

Object recovery rows (REM-PARITY §10.3) differ by representation,
deliberately. A **plaintext** object's row carries the manifest anchors
(`manifest_first_chunk_lba` — an inner `BodyLba` — `manifest_size_bytes`,
`manifest_chunk_count`, `manifest_sha256`). An **encrypted** object's row
MUST NOT carry those manifest anchors. [REMPARITY] owns the exact row schema,
and a Writer (REM-PARITY §2.2) producing the tape binding MUST honor it.

An encrypted object's catalogless recovery uses its own envelope header and
key frame (REM-ENCRYPT §5.10). REM-ENCRYPT §12.5 owns the confidentiality
rationale. Plaintext objects remain fully recoverable keyless (Section 4.10).

### 8.3. File Binding

The stored bytes are stored as one regular file, with the default extension
`.rem-object` for both representations (Section 3.4 disambiguates).
Recommended practice for staging and publishing a file-bound copy is described
in the REM Implementation and Operations Guide, under “Staging, commit and
durability”.

### 8.4. Object-Store Binding

The stored bytes are stored as one object or blob. Range reads (Section 6.2
and REM-ENCRYPT §6.3) make PFR efficient without downloading whole objects.
Recommended practice for uploading a copy to an object store is described in
the REM Implementation and Operations Guide, under “Ingest”.

## 9. Relationship to the Parity Layer

The parity layer [REMPARITY] protects tape-resident stored blocks with
Reed-Solomon parity, block CRCs, parity-epoch sidecar tape files, a filemark
map, and the bootstrap and the terminal replicas. **The parity construction
and geometry are independent of this document**; what REM-OBJECT relies on,
and what it adds to the terminal replicas' Object recovery rows, is:

1. **Parity is computed over stored bytes** — the ciphertext, when the copy is
   encrypted. The order is: build → (seal) → parity. The parity layer protects
   bytes regardless of content and needs no keys, ever; recovery of damaged
   blocks of an encrypted object proceeds keyless.
2. Within one object's tape file there are no parity or bootstrap blocks; the
   object's stored blocks are contiguous (stored `BodyLba` 0..N−1). Parity
   epochs span objects; sidecars land between tape files. None of this is
   visible in, or part of, the object's stored bytes.
3. An object is *committed* when the parity layer's durable object-commit
   operation completes ([REMPARITY]); neither representation defines an
   in-band commit marker. REM-ENCRYPT completion framing is not a commit
   barrier.
4. **Informative:** REM-PARITY §10.3 defines the encrypted-object Object
   recovery row, while Section 8.2 states the obligation this document places
   on the REM-PARITY Writer. This relationship is additive and does not change
   the parity construction.

## 10. Versioning and Extensibility

This document has four independent version axes:

| Axis | Value | Where it is recorded |
| --- | --- | --- |
| Document version | 1.0 | This document only; it is not stored in an object |
| Stream format identifier | `rem-object-v1` | The global `REMANENCE.format_id` keyword |
| Stream schema version | `1.0` or `1.1` | The global `REMANENCE.schema_version` keyword |
| Manifest schema version | `1` | The manifest's `schema_version` key |

The four axes MUST NOT be used as proxies for one another. REM-ENCRYPT §10 and
the REM-PARITY Status section state those documents' own axes.

`REMANENCE.schema_version` is `1.0` when no xattr is preserved and `1.1`
when any xattr is preserved. Both values are defined by document 1.0. The
numeral is a feature gate, not a document version: a future revision 1.1 of
this document would not imply stream-schema `1.1`, nor the reverse. Extension containers and the `object_metadata`
inventory do not affect this gate. Both stream-schema values use
`REMANENCE.format_id = rem-object-v1` and manifest
`schema_version = 1`.

A Reader of `rem-object-v1` gates on stream-schema major 1 and ignores unknown pax
keywords, manifest keys, and extension-container keys. Extensions MUST NOT
change an existing field's meaning, alignment, entry semantics, compression
or encryption gates, or any other rule enforced by this document. Such a
change requires a new stream `format_id`. REM-ENCRYPT §10 independently owns
encrypted-envelope versioning and registries.

A Repacker (Section 2.2) MUST reproduce unknown manifest keys and unrecognized
extension-container members unchanged under the Section 4.7.1 canonical
encoding. Ignoring a key on read does not license dropping it on rewrite. A
Repacker MUST likewise re-emit all unknown pax keywords unchanged. For the
preserving-rewrite case this strengthens the Section 4.4.3 SHOULD to a MUST. A
Repacker that recognizes the `xattrs` map and selectively strips attributes is
performing a declared policy action, not a transparent rewrite, and thereby
changes `plaintext_digest`. Because every extension is ancillary (Section
4.7.5), a minimal Consumer that ignores all extension data and recovers
payload bytes and structure remains conformant for the roles it claims
(Section 14).

## 11. Errors

The error names below are normative for the test-vector manifests (Section
13); surface syntax is not. I/O failures MUST remain distinguishable from
format violations. Callers can then tell storage problems from invalid
objects. Section 12.9 describes the hazards of hostile input.

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
IncompleteBlockWrite      Section 4.9 Builder failure
SourceIo                  payload source read failure (not a format violation)
TapeIo                    block sink/source failure (not a format violation)
```

### 11.2. Envelope Errors (in REM-ENCRYPT)

## 12. Security Considerations

### 12.1. Per-Object Key Uniqueness (in REM-ENCRYPT)

### 12.2. Key Separation and Nonce Safety (in REM-ENCRYPT)

### 12.3. Binding Without AAD (in REM-ENCRYPT)

### 12.4. Fail-Closed (in REM-ENCRYPT)

### 12.5. Confidentiality Boundary, Public Facts, and Catalog Trust (in REM-ENCRYPT)

### 12.6. Plaintext Copies Are Not Self-Authenticating

A plaintext REM-OBJECT object provides integrity plumbing, not authentication: an
attacker who can rewrite the medium can rewrite payloads, pax hashes, and the
manifest consistently. A lone plaintext object whose hashes verify internally
proves only self-consistency. The trust anchor is external — the catalog's
`stored_digest` and, on tape, the Object recovery row's `manifest_sha256`
(REM-PARITY §10.3; plaintext rows, Section 8.2). Encrypted copies are
authenticated as specified by REM-ENCRYPT, subject to its
non-committing-AEAD caveat (REM-ENCRYPT §12.7).

The off-tape catalog is a separate trust domain: it holds external anchors and
may hold cleartext paths and per-file rows even when a stored copy is
encrypted. REM-ENCRYPT §12.5 states the encrypted-copy public-facts
consequences and the consequences of the minimal encrypted Object recovery
row.

### 12.7. Non-Committing AEAD (in REM-ENCRYPT)

### 12.8. Key Rotation and Epoch Longevity (in REM-ENCRYPT)

### 12.9. Hostile-Input Posture

Stored bytes come off removable media and networks, and in both
representations they are untrusted. The plaintext stream's acceptance checks
are required elsewhere in this document. Section 4.3.3 requires a Reader to
verify the checksum of every header record. Section 4.4.1 requires every pax
record length to be checked against the header payload that remains.
Section 4.2 limits a Reader to the declared number of blocks, and Section 4.9
makes an end of object inside a declared payload the error `TruncatedPayload`.
The `chunk_size` and block count arrive from the catalog or, on tape, from the
bootstrap and the Object recovery row (Section 4.2), so they are semi-trusted
inputs. A reader that
panics, exhausts memory, or allocates by a size it has not yet checked can be
brought down by a single hostile object, even though it reads every valid
object correctly. Recommended practice for handling hostile media is described
in the REM Implementation and Operations Guide, under “Handling hostile
media”. REM-ENCRYPT §12.9 describes the envelope-specific bounds.

### 12.10. Path Traversal

Native entry paths cannot represent traversal: Section 4.6.6 forbids absolute
paths and `.`/`..`/empty components at write time, and Readers reject
violations (`InvalidPath`), so a conformant entry path is always a clean
relative path. Symlink targets are different: they are opaque OS strings and
may be absolute, contain `..`, or be dangling.

A restore destination lies outside the object, and a hostile object can aim at
it. In the classic archive attack, an earlier symlink entry creates
`dir -> /outside` and a later regular entry writes through `dir/file`. A
restoring tool that follows symlinks already present in the destination then
writes outside its restore root. A Restoring Consumer MUST create symlink
entries as symlinks, without dereferencing their targets. A Restoring Consumer
MUST materialize a hardlink's primary before creating the hardlink (`link(2)`)
to the already-restored primary.

**Native path mapping.** Section 4.6.6 makes an entry path a clean
`/`-separated relative path, but that grammar is validated against POSIX
semantics only. On a non-POSIX target filesystem the same bytes can denote
something else. On Windows, a component such as `..\outside` embeds a
separator the REM-OBJECT grammar never inspected. Windows also reads a value
like `C:\x` or `\\host\share\x` as an absolute path, on a drive or on a UNC
share. Case-folding, such as Windows case-insensitivity, and Unicode
normalization, such as NFC and NFD, can also collapse two REM-OBJECT-distinct
entry paths onto one native target. An entry's native destination is the path
it would occupy on the target filesystem after that filesystem's separator,
case-folding and Unicode-normalization rules are applied. A Restoring Consumer
that maps entry paths onto a native filesystem MUST reject or report, never
silently overwrite, any entry whose native destination collides with a
destination that another entry in the same object has already produced.
Framing-layer acceptance of a path is a necessary check, not a sufficient
safety claim. An inner stream recovered through REM-ENCRYPT is parsed and
restored under these same rules (REM-ENCRYPT §5.10). Stock tar extraction has
its own security model. REM-OBJECT's standard-tool fallback is faithful, not
inherently sandboxed.

Preserved xattrs are equally untrusted. Attributes such as Linux
`security.capability`, `security.*`, `trusted.*`, and POSIX ACL attributes can
change privilege or access-control state — a restored `security.capability`
is a privileged binary. An extension (Section 4.7.5) can carry platform
metadata with the same effect when it is applied. A Restoring Consumer MUST
treat attribute values as opaque bytes. Section 4.7.3 requires a failure to
apply an attribute to be reported.

Recommended practice for restoring onto a host is described in the REM
Implementation and Operations Guide, under “Restoring onto a host”.

### 12.11. Threat Model and Secret Handling (in REM-ENCRYPT)

### 12.12. Disclosure in Published Plaintext Objects

A plaintext REM-OBJECT object provides integrity plumbing (Section 12.6), not
confidentiality: its manifest is readable with any CBOR tool (Section 4.10).
Publishing a plaintext object discloses, at minimum: every entry path and
(possibly absolute or dangling) symlink target; the directory tree and
hardlink topology; file sizes and the file count; `mtime` and `executable`
values; `file_id`, `object_id`, `caller_object_id`, `write_timestamp`, and
`chunk_size`; every captured attribute value; and the `object_metadata`
inventory itself, which names the non-`user.` namespaces and extensions
present (revealing, for example, macOS or Windows origin). The encrypted
representation seals the manifest inside the REM-ENCRYPT envelope and does
not have this
plaintext-disclosure exposure. The Verifier-validated inventory
(Section 4.7.6) is the intended first-pass screening surface, but does not
itself bound value-level disclosure. The standard-tool recovery path (Section
4.10) inherits the host tool's security model, and it restores symlinks
faithfully. The format's protection there is limited to keeping
privilege-changing metadata, such as ownership, setuid/setgid mode and
extended attributes, in a form no standard `tar` applies to target files. It
is not a sandbox.

## 13. Test Vectors

Static test vectors are distributed alongside this specification, each with a
manifest entry recording inputs, the expected values pinned below, and — for
negative vectors — the expected Section 11 error name. Vectors use small
`chunk_size` values (e.g. 4096) so full object byte streams are practical to
pin. At least one vector MUST use `DEFAULT_CHUNK_SIZE`.

The authoritative companion archive is `remanence-test-vectors.tar`, SHA-256
`77be73e780e9ff2c265c8357b6ba684b4c69800213820ae1331850f742b1d83d`.
The authoritative archive digest names this specific version of the frozen
conformance distribution. Later additive vector entries are versioned
supplements carrying their own digest; they do not mutate the conformance
target named by an earlier archive digest.
Its `MANIFEST.tsv` inventories every contained vector manifest and generated
artifact, `CHECKSUMS.sha256` authenticates them, and the included `verify.py`
checks the archive without a source checkout. It contains plaintext and xattr
positive objects and plaintext negative manifests. REM-ENCRYPT §13 owns the
envelope vectors in the same archive. The archive's checksums, rather than abbreviated values
in this prose, are the byte-identity authority. Payload digests are
independently checkable with `sha256sum`.

### 13.1. Plaintext-Stream Positive Vectors

The plaintext suite MUST include at least the vectors in the following table.
Each is named by its manifest's `vector_id`; the archive's `MANIFEST.tsv` lists
the manifests.

| Vector | What it pins |
| --- | --- |
| REM-OBJECT-TV-EMPTY | An empty object: the global header, the manifest and EOF only |
| REM-OBJECT-TV-EMPTY-FILE | An empty file: `chunk_count` 0, absent `first_chunk_lba`, and `null` in the manifest |
| REM-OBJECT-TV-ONE-BYTE | A one-byte file |
| REM-OBJECT-TV-BOUNDARY | A chunk-boundary set: payload sizes `chunk_size − 1`, `chunk_size`, `chunk_size + 1`, and one multi-chunk size |
| REM-OBJECT-TV-PATHS | Pathological paths: a non-ASCII path and a > 100-byte path, both exercising `PAX_PATH_PLACEHOLDER`, and a 100-byte portable path stored inline |
| REM-OBJECT-TV-METADATA | Full metadata: entries with `mtime`, with `executable=true` at mode 0755, and with `executable` unsupplied, which the manifest records as `null` |
| REM-OBJECT-TV-ORDER | A multi-file object whose entries are not in alphabetical order, pinning caller-order preservation |
| REM-OBJECT-TV-NONREGULAR | Non-regular entries: a symlink with its target and an empty directory |
| REM-OBJECT-TV-NONREGULAR | A long link target: a symlink whose target exceeds 100 bytes, exercising `PAX_LINK_PLACEHOLDER` and pax `linkpath` |
| REM-OBJECT-TV-HARDLINKS | A hardlink, primary and link, restoring to one shared inode |
| REM-OBJECT-TV-HARDLINKS | A long link target: a hardlink whose target exceeds 100 bytes, exercising `PAX_LINK_PLACEHOLDER` and pax `linkpath` |
| REM-OBJECT-TV-MANIFEST | The exact manifest CBOR bytes and `manifest_sha256` for a fixed input set: the canonical-manifest byte-identity vector, the cross-implementation determinism gate of Section 4.7.1 |
| REM-OBJECT-TV-PORTABLE-CORE-ONLY | A portable-core-only object: `user.` attributes only, an empty `object_metadata`, and `REMANENCE.schema_version` pinned |
| REM-OBJECT-TV-NONUSER-ATTRIBUTE | An object with a non-`user.` attribute and a correct inventory |
| REM-OBJECT-TV-EXT-MEMBER | An object with an unknown reverse-DNS `ext` member, for which a minimal Consumer recovers the payloads and ignores the member, and a Repacker reproduces it under canonical encoding |
| REM-OBJECT-TV-ATTRIBUTE-EXT-COMBINED | A non-`user.` attribute and an `ext` member combined, with the two-array inventory pinned exactly |

For each vector, the manifest pins the exact full object byte stream or, for a
large vector, `full_object_sha256` and either the first object block bytes or
`first_block_sha256`. It also pins `stored_size_blocks`, every entry's
`(pax_header_offset, data_offset, first_chunk_lba, chunk_count, pad_spaces)`,
the manifest CBOR bytes, and `manifest_sha256`.

The suite also includes a manifest carrying an unknown top-level key in valid
manifest-profile CBOR (`unknown-extra-key-accepted`, in
`negative-manifest.json`). A Consumer MUST accept it (Section 4.7.2,
obligation 3).

Where a positive manifest carries an `expected.default_restore` object, its
fields `skipped_xattrs`, `applied_privileged_xattrs`, `carried_extensions` and
`reported_values` record the behaviour of a restore that follows the
recommended defaults described in the REM Implementation and Operations Guide,
under “Restoring onto a host”. They are informative for conformance. A
conformant Restoring Consumer need not reproduce them. In the vector with a
non-`user.` attribute, for example, `skipped_xattrs` names that attribute and
`reported_values` is `false`. The `file_layouts` field and every other expected
field keep their normative force.

### 13.2. REM-OBJECT-TV-P1 — Plaintext Object

Inputs (complete; `manifest_file_id` is the manifest entry's `file_id`):

| Input | Value |
| --- | --- |
| `chunk_size` | 4096 |
| `object_id` | `00000000-0000-4000-8000-000000000001` |
| `caller_object_id` | `rem-object-tv-1` |
| `write_timestamp` | `2026-01-01T00:00:00Z` |
| `metadata_preservation` | `minimal` |
| `manifest_file_id` | `00000000-0000-4000-8000-0000000000ff` |
| File 0 | `path` = `a/hello.txt`, `file_id` = `00000000-0000-4000-8000-000000000010`, no `mtime`, no `executable` |
| File 0 contents | The 18 ASCII bytes `hello, rem-object` + LF |
| File 0 expected `file_sha256` | `f3daa1e791237b4fc5586b0a1a6eefdfb98e0821ff5cef14819f38b9fc4c1a5f` |
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
| Manifest: pax record at offset | 13312; manifest CBOR size 554 bytes |
| Manifest: `first_chunk_lba`, `chunk_count` | 4, 1 (data at byte 16384) |
| Tar EOF at | 17408; `total_size_bytes` = 20480; `projected_size_blocks` = 5 |

Pinned outputs: the exact full object byte stream; the manifest CBOR bytes;
`manifest_sha256 =
ecbf48e48cc11a78b9d6ae9dd7b5e938724ebdb39834a70f17bde17d3eb133da`;
and `plaintext_digest = stored_digest =
d59a4a3e4cf2c447c8ed402b109fbb4060ca84dc5b1cebbdc3acb8ca62d8888c`.
Exact bytes are pinned in the companion archive.

### 13.3. Encrypted-Object Vectors

The vectors for encrypted objects — positive, component, range, and negative —
are owned by REM-ENCRYPT §13 and are not restated here.

### 13.4. REM-OBJECT-TV-D1 — Default Chunk Size

One vector MUST use `DEFAULT_CHUNK_SIZE`. Inputs: `chunk_size` 262144;
`object_id` `00000000-0000-4000-8000-000000000002`; `caller_object_id`
`rem-object-tv-d1`; `write_timestamp` `2026-01-01T00:00:00Z`;
`metadata_preservation` `minimal`; `manifest_file_id`
`00000000-0000-4000-8000-0000000000fe`; one file `v.bin`, `file_id`
`00000000-0000-4000-8000-000000000012`, contents 262145 bytes with byte `i` =
`i mod 256` (expected `file_sha256`
`c35991ad254f48ff8b02becb9f0cc56581e86a0b477b13e5ebb0030a3b91c848`,
`chunk_count` 2). The plaintext object is 1048576 bytes (4 blocks);
its manifest is 358 bytes at inner `BodyLba` 3;
`manifest_sha256 =
2e2de7f397bcf83237edf308432c4bff0a7922150177851749666e35fe660599`;
and `stored_digest = plaintext_digest =
5d07a7aca146a80dfae22f06de976924a6f5c95aceff119b057894a3ab8e1bf5`.
REM-ENCRYPT §13.2 specifies the encrypted copy.

### 13.5. Xattr Vectors

`rem-object/objects/rem-object-tv-xattrs.rem-object` and its manifest pin an xattr round trip. The
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
`stored_digest`. The Builder's no-xattr path remains schema `1.0` and emits empty
containers as required by Section 4.7.3.

### 13.6. Negative Vectors

Each contains exactly one fault and asserts the mapped error.

**Plaintext stream, Builder side.** These vectors are constructed through the
Builder's input.

| Fault | Expected Section 11 error |
| --- | --- |
| Duplicate path | `InvalidInput` |
| Duplicate `file_id` | `InvalidInput` |
| Manifest `file_id` colliding with a payload `file_id` | `InvalidInput` |
| Reserved `_remanence/` path | `InvalidInput` |
| Control character in path | `InvalidInput` |
| Each non-canonical path shape (`/abs`, `a/../b`, `./a`, `a//b`, `a/`) | `InvalidInput` |
| Malformed `mtime` | `InvalidInput` |
| Streamed payload with wrong hash | `InvalidInput` |
| Streamed payload with wrong size | `InvalidInput` |
| Non-multiple-of-512 `chunk_size` | `InvalidInput` |
| Symlink or directory with nonzero size | `InvalidInput` |
| Symlink missing target | `InvalidInput` |
| Directory path without trailing slash | `InvalidInput` |
| A hardlink whose target is absent or not a regular-file primary | `InvalidHardlinkTarget` |

**Plaintext stream, Reader side.** These are byte vectors.

| Fault | Expected Section 11 error |
| --- | --- |
| Wrong `REMANENCE.format_id` | `UnsupportedFeature` |
| Schema major 2 | `UnsupportedFeature` |
| Missing `REMANENCE.compression` | `Parse` |
| `REMANENCE.compression=gzip` | `UnsupportedFeature` |
| `REMANENCE.encryption=aes-256-gcm` | `UnsupportedFeature` |
| Declared `REMANENCE.chunk_size` disagreeing with the supplied geometry | `ChunkSizeMismatch` |
| Corrupted header checksum | `UstarChecksumMismatch` |
| Single zero EOF record | `Parse` |
| Unknown typeflag | `UnsupportedTarTypeflag` |
| Misaligned nonzero payload | `ChunkAlignmentViolation` |
| Traversal-shaped effective path | `InvalidPath` |
| An entry after the manifest | `Parse` |
| One flipped payload bit, read in restore mode | `FileDigestMismatch` |
| Truncated payload | `TruncatedPayload` |
| Truncated pax body | `TruncatedPayload` |
| Pax record length out of bounds | `PaxRecordMalformed` |
| Pax record missing `=` | `PaxRecordMalformed` |
| Pax record missing trailing newline | `PaxRecordMalformed` |
| Pax value with control character | `PaxRecordMalformed` |
| Non-UTF-8 pax value | `PaxRecordMalformed` |

**Manifest.**

| Fault | Expected Section 11 error |
| --- | --- |
| Non-canonical key order | `Cbor` |
| Non-shortest integer encoding | `Cbor` |
| Indefinite-length item | `Cbor` |
| Float | `Cbor` |
| Tag | `Cbor` |
| Duplicate map key | `Cbor` |
| `schema_version` 2 | `ManifestInvalid` |
| `file_sha256` of wrong length | `ManifestInvalid` |
| Nesting depth exceeding `MANIFEST_MAX_DEPTH` | `Cbor` |
| Manifest bytes disagreeing with the anchor | `ManifestDigestMismatch` |
| Manifest `chunk_size` disagreeing with the global header | `ManifestInvalid` |
| Two `file_entries` sharing a `path` | `ManifestInvalid` |
| Two `file_entries` sharing a `file_id` | `ManifestInvalid` |

**Additive negative vectors.**

| Fault | Expected Section 11 error |
| --- | --- |
| An inventory that disagrees with the entries: a non-`user.` attribute is present but the inventory is empty or wrong | `ManifestInvalid` |
| A non-canonical `ext` value | `Cbor` |
| A manifest tamper with constant payload, with an anchor present: a repointed `path`, swapped `file_sha256`, or altered `first_chunk_lba` | `ManifestDigestMismatch` |

Each additive negative vector pins the typed Section 11 error name and names
the affected digest, `plaintext_digest`, which equals `stored_digest` for a
plaintext copy. Each manifest-tamper vector pins a distinct
`plaintext_digest`. A restore-report vector reaches EOF without a manifest and
asserts the typed `MissingManifest` report rather than silent absence.

## 14. Conformance

An implementation conforms only for the roles it claims. A conforming Builder
implements the canonical stream and every feature it emits. A Reader MAY
decline xattr restore. A Reader that declines it MUST still preserve file-byte
recovery, ignore the `xattrs` maps, and report that the attributes were not
applied. An implementation claiming an encrypted role also conforms to
REM-ENCRYPT and its Section 13 vectors.

Conformance evidence MUST include:

1. byte-exact agreement with the applicable positive objects and manifests in
   the Section 13 archive;
2. the applicable typed rejects in the negative manifests, including the
   Core manifest and plaintext-stream sets;
3. GNU tar, bsdtar, and Python `tarfile` extraction equality for plaintext
   objects;
4. plaintext range recovery across a chunk boundary;
5. failure without reporting a completed object on injected size, digest, and
   I/O failures; and
6. the applicable vectors of Section 13 for:
   - the portable core;
   - extension containers;
   - the object inventory;
   - Repacker preservation; and
   - manifest tampering.

The `expected.default_restore` fields that Section 13.1 describes as
informative are not part of this evidence.

The archive SHA-256 in Section 13 identifies one specific version of the
frozen vector distribution. Changing an existing entry's byte encoding or
expected result requires a revision classified under the Status policy — a
minor revision at least, a new major if any previously valid object is
affected; an erratum may not alter any entry. Additive
entries are published as versioned supplements carrying their own digest and
do not mutate the frozen conformance target named by an existing digest.

## 15. IANA Considerations

The identifiers this specification defines—the `rem-object-v1` stream format
identifier, the `REMANENCE.` pax keyword namespace, the `"xattrs"`
preservation key, the `ext` indirection key, and the `object_metadata` inventory keys
(`attribute_namespaces`, `extensions`) — are assigned by this document and
governed by its versioning rules (Section 10).

This document establishes no IANA registry. Extension names (Section 4.7.5)
use permissionless reverse-DNS naming and require no central allocation. A
community-maintained advisory list MAY record registered short names. That
list is not a precondition for conformance. Reverse-DNS extension names apply
to manifest extension containers only and MUST NOT appear as pax keywords.

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
- [RFC8949] — Bormann, C. and P. Hoffman, "Concise Binary Object
  Representation (CBOR)", STD 94, RFC 8949, December 2020,
  <https://www.rfc-editor.org/info/rfc8949>.
- [FIPS180-4] — National Institute of Standards and Technology, "Secure
  Hash Standard (SHS)", FIPS PUB 180-4, August 2015 (defines SHA-256),
  <https://doi.org/10.6028/NIST.FIPS.180-4>.
- [POSIX-PAX] — IEEE Std 1003.1-2017 (POSIX.1-2017), Shell and Utilities
  volume, `pax` utility, "pax Interchange Format" (ustar and pax extended
  headers), <https://pubs.opengroup.org/onlinepubs/9699919799/>.
- [REMPARITY] — "Rem Tape Parity (REM-PARITY) Format", Version 1.0 or any
  later 1.x revision (interchangeable for this document's purposes by
  REM-PARITY's own change policy); this revision was published against
  REM-PARITY 1.0.0. Companion specification: the parity layer of Sections
  8.2 and 9. Normative only for implementations of the Section 8.2 tape
  binding.
- [REMENCRYPT] — "REM-ENCRYPT", Version 1.0 or any later 1.x revision (published against 1.0.0), companion specification defining the
  encrypted representation named by this document.

### 16.2. Informative References

- [PREMIS] — PREMIS Editorial Committee, "PREMIS Data Dictionary for
  Preservation Metadata", Version 3.0, June 2015, Library of Congress,
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

This appendix summarizes the Section 13.2 alignment result. The frozen
companion archive is the byte-level conformance authority.

For REM-OBJECT-TV-P1 with `chunk_size = 4096`, the global header occupies
bytes 0–1023. File 0's pax record begins at 1024; its 1792-space
`REMANENCE.pad` value places the 18-byte payload at byte 4096
(`BodyLba` 1). File 1's pax record begins at 4608; its 2300-space pad places
the 5000-byte payload at byte 8192 (`BodyLba` 2).

The manifest pax record begins at 13312. Its 1718-space pad places the
554-byte deterministic-CBOR payload at byte 16384 (`BodyLba` 4). Tar EOF
begins at 17408 and ends at 18432; final fill produces
`total_size_bytes = 20480`, or five blocks.

## Appendix B. Design Rationale (Informative)

This appendix records the reasoning behind non-obvious decisions, so future
revisions do not silently reverse them.

### B.1. Encryption Is an Envelope, Never an In-Stream Flag

`REMANENCE.encryption` is permanently `none`; confidentiality is the
REM-ENCRYPT envelope around the stream. Flagging encryption inside the global
header would break the shared `plaintext_digest` (the two copies' canonical
bytes would differ), break standard-`tar` extractability of the plaintext
copy, and place the marker inside the very bytes it claims are encrypted.

### B.2. Two Identities: Logical and Physical

`plaintext_digest` (SHA-256 of the complete canonical object) is the logical
identity, shared by a plaintext and an encrypted copy of one object;
`stored_digest` (SHA-256 of one copy's stored bytes) is the physical identity
that backends scrub keyless. For a plaintext copy the two coincide. Copies
share the logical identity if and only if they wrap identical canonical
bytes ("build
once, fan out"); rebuilding from the same inputs with a new `object_id`,
timestamp, or `chunk_size` yields a new object, and per-file `file_sha256` is
the cross-rebuild invariant.

### B.3. `stored_digest` Is External

A digest over the complete stored bytes cannot live inside them, and a
truncated in-band variant would be a second, weaker integrity story. A
cataloged `object_id` lets a scrubber look up the trusted external digest.
Keyless scrub = external
`stored_digest` + (on tape) the parity layer's block CRCs.

### B.4. No Unencrypted Envelope

The plaintext representation is the bare canonical stream, not a stream inside
an extra wrapper. Such a wrapper would break standard-`tar`
extractability — the plaintext copy's reason to exist — in exchange for
framing the stream already provides (self-description, digests).

## Appendix C. Revision History (Informative)

Entries are newest first: date · version · kind (erratum / minor / major) ·
effect on conformance.

- **2026-09-26 — 1.0.0-draft.4 — in preparation.** The preparing copy of the
  next revision. This entry is completed as the revision's changes are made.
  Section 1.6 is added. It states which rules belong in this document and
  points to the REM Implementation and Operations Guide for the rest. A rule
  about what an implementation does stays only if ignoring it would do one of
  three things. It would produce bytes another conformant reader cannot read
  or would misread. It would let two conformant readers reach different
  conclusions about what a medium or an object contains. Or it would let a
  tool claim something the bytes do not support. The same section appears,
  word for word, as the last subsection of Section 1 of REM-OBJECT,
  REM-ENCRYPT and REM-PARITY.

  The rules that Section 1.6 places outside this document have now left it.
  Most went to the REM Implementation and Operations Guide, whose first
  revision this change writes. They concern protecting the host a restore
  writes onto, surviving hostile input, staging and durability, catalogs,
  keeping attribute values out of logs, optional verification, scrub and
  repair, ingest, and which directory entries a Builder emits. Statements about
  the reference implementation's reader and its wrapper tooling went to that
  implementation's documentation; Appendix E keeps the wrapper convention and
  its field meanings. Sections 7.3 and 7.5 lost all of their text; each keeps its
  heading over one sentence that points to the Guide.

  Some rules stay in a changed form. Section 4.7.2 obligation 1 now says that
  a Consumer does not rely on a manifest field it has not verified against an
  anchor digest, instead of prescribing the order of work. Its failure is
  still `ManifestDigestMismatch`. The portable core and the extension tier are
  now defined by attribute namespace rather than by what a restore applies by
  default, and Section 4.7.3 now defines carry-only. Section 12.10 keeps the
  rules on reproducing symlinks, hardlinks and colliding paths, and describes
  the hazards whose handling moved. The file extension `.rem-object` is now
  named without a keyword. Sections 13.1 and 14 now say that the
  `expected.default_restore` fields of the vector manifests are informative
  for conformance.

  No byte of the format changed, and no valid object or vector changed. This
  document no longer requires a conformant tool to protect the host it
  restores onto, to survive hostile input, or to stage its output durably;
  the Guide recommends each. REM-ENCRYPT's and REM-PARITY's rules of the same
  kind have since left those documents too, as their own revision histories
  record.

  The readability pass then changed the text as follows. No byte of the format
  changed, and no valid object or vector changed.

  - The Abstract and Table of Contents now stand above Section 1, and the
    Identifiers table no longer repeats the two DOIs that the Status table
    carries.
  - Section 1.1 now says which sections this document and REM-ENCRYPT number
    together. A placeholder heading marks each section or subsection that
    REM-ENCRYPT holds.
  - Section 1.1's list of tape-binding clauses now names every section whose
    parity-layer reference binds the tape: Sections 3.3, 4.2, 4.5.1, 4.7, 6.5,
    8.2, 9 and 12.6.
  - Section 4.2 is now “Chunks and `chunk_size`”, Section 4.9 is “Builder,
    Planner, and Reader Obligations”, and Section 7.3 has lost “(Deployment
    Obligation)”, which described text that had moved to the Guide.
  - The statement of what integrity a range read can claim moved from Section
    4.9 to a new Section 6.5. It now names the Restorer for the duty to report
    which case applies, and keeps its case (c) prohibition on every
    implementation.
  - The producing role is the Builder throughout. “Writer” now means only the
    REM-PARITY Writer, in the tape-binding clauses.
  - “Chunk” replaces “body block” as the name of the unit inside the object.
    Section 2.3 defines the two `BodyLba` indexes in terms of chunks and
    stored blocks, and gains a table of the three units.
  - Statements that the tape bootstrap carries an object's manifest anchors,
    identifier or block count, or that it is replicated or parity-protected,
    described the generation-1 tape layout. They now name the terminal
    replicas' Object recovery rows (REM-PARITY §10.3), which carry those facts
    in generation 2. Sections 4.2 and 12.9 name the bootstrap only for the
    tape's block size, which equals `chunk_size`. The requirements they state
    are unchanged.
  - Section 1.1 now limits “never reused for different bytes” to the version
    string of a deposited revision. A copy in preparation keeps its version
    string while its text changes.
  - Section 13.1 lists its vectors in a table by their archive `vector_id`,
    and Section 13.6 lists its faults in tables, each with its expected
    Section 11 error. The case of an unknown manifest key that a Consumer
    accepts moved from Section 13.6 to Section 13.1. The four expected errors
    that the prose of Section 13.6 also stated as requirements remain required
    where their checks are defined (Sections 4.7.1, 4.7.2, 4.7.6 and 4.9).
  - Section 4.4.1 states the pax length iteration as numbered steps with two
    worked examples, and Section 4.6.3 states the pad-length search as
    numbered steps. Section 4.9 gives its typeflag dispatch as a table,
    Section 7.4 lists the Verifier's checks, Section 10 gives its version axes
    as a table, and Section 14 lists its vector families one per line.
  - An explanation of why a rule exists now stands in its own paragraph,
    opening with *Rationale.*, which Section 2.1 declares informative.
  - Sentences that stated more than one requirement, or a requirement and an
    aside, were split. Where the role a requirement binds was only implied,
    the sentence now names it.
  - Errata: Sections 8.3 and 8.4 gain the verb they lacked; Section 4.7.5 no
    longer claims to amend Section 4.7.2; Section 14 no longer says “safely”,
    which lost its referent when the host-safety rules moved to the Guide;
    Section 12.10 no longer calls a Windows path such as `C:\x`
    drive-relative; and Appendix E cites REM-ENCRYPT §6 in the document's
    usual form.
- **2026-09-10 — 1.0.0-draft.3 — review-draft errata.** Adds Appendix E,
  an informative description of the capacity cost the alignment rule imposes
  on very small entries and of the `.remwrap.tar` / `.remwrap.idx` wrapper
  convention the reference implementation layers above the format, including
  how one inner file is recovered from a wrapper. No normative text changed;
  no valid object or vector changed.
- **2026-09-06 — 1.0.0-draft.2 — review-draft errata.** Clarifies that shared
  numbering extends to subsections, removes a dead REM-PARITY Section 18
  cross-reference, states the reserved-but-not-deposited publication status,
  and makes explicit that a Reader may reject nonzero post-EOF fill while a
  Verifier must check it. No valid object or vector changed.
- **2026-07-31 — 1.0.0-draft.1 — review draft.** The first revision of this
  document published under its own identifier, and the first offered for public
  review. Comments close 30 April 2027; the document freezes 31 July 2027.

  Text substantially the same as the copy distributed on 2026-07-25 (below),
  with these changes:

  - It introduces the Status section's change policy, which the earlier copy
    did not carry in any form.
  - It records this document's concept DOI.
  - It adds this revision history.
  - It restates the dependency on REM-PARITY as stability against the 1.x
    line.
  - It reclassifies Section 14's vector-change rule from erratum to revision.
  - It de-couples the Section 4.4.3 and Section 10 wording from document
    minors.
  - It warns explicitly against confusing the stream-schema numeral with the
    document version.
  - It narrows the Section 4.7.3 empty-map requirement so it no longer forbids
    the extension container its own schema permits.
  - It rebuilds the Table of Contents so a renderer cannot silently renumber
    it around the deliberate Section 5 gap.
  - It adds the worked classifications, the bytes-to-defining-revision lookup
    paragraph and the vectors-are-anchors rule.

  No object written against the earlier copy is affected, and no published
  vector changed. Where a correction touches a wire statement, the statement as
  written could not be satisfied by any implementation that also read a
  conformant object, so no conformant implementation loses anything.
- **2026-07-25 — pre-release copy.** A copy of this document was distributed
  inside software release v1.0.0 (Zenodo 10.5281/zenodo.21551571, a *software*
  record). It was not deposited or citable as a document: it carried no DOI of
  its own — its metadata table read "DOI: assigned at first release" — no
  change policy and no revision history, and it was reachable only by
  unpacking the source archive.

## Appendix D. Open Items (Informative)

**This list is a snapshot**, taken when this revision was fixed. It is not
updated afterwards: the bytes of a published revision are immutable and their
digest is cited elsewhere. Items raised after this date are recorded in the
live list at **<https://archivetech.org/spec/issues>**, which is current —
consult it before reporting anything, and expect it to hold more than appears
here. The next revision carries a new snapshot, and its revision-history entry
says which items it resolved.

Listing an item here is not a commitment to act on it. An item marked
*accepted* will be addressed in a future revision; *deferred* means we judge it
real but not yet ripe; *declined* means we considered it and decided against,
with the reason given so the decision can be argued with rather than merely
discovered. Comment is invited on everything, and most of all on the items
marked so.


**RO-1 · An independent implementation from the prose alone · open, and the
review we most want.** The central claim of this document is that an
implementation built from its text alone reproduces the published vectors.
Nobody outside the project has tested that. A disagreement between your reader
and the vectors is the most valuable report this window can receive.

**RO-2 · An embedded copy of the specification on tape · deferred.** Carrying
the specification as an object on the medium it describes would remove the
dependency on an external retrieval path. It would require a reserved object
identity in this document and classification support in REM-PARITY; it is
recorded there as RP-3. *Comment invited.*


## Appendix E. Packing Many Small Files (Informative)

This appendix describes a cost that follows from the alignment rule, and the
convention the reference implementation uses to avoid it. Nothing here is
normative. An object that never uses the convention is conformant, a Reader
that does not recognise it is conformant, and the two members the convention
produces are ordinary regular entries governed entirely by Section 4.

### E.1. What Alignment Costs

Section 4.6.3 requires every non-empty entry's payload to begin on a chunk
boundary, and achieves it inside the entry's own pax header, where standard
tools do not see it. The space between the end of one payload and the next
chunk boundary is taken up by the next entry's headers and pad, so the stream
advances by at least one chunk for every non-empty entry, whatever its size.
On tape `chunk_size` is the tape block size, so a 1 KiB file occupies
256 KiB, 512 KiB, or 1 MiB of the stream depending on the block size the
cartridge was written with, and nearly all of that space carries nothing.
For entries of a few hundred kilobytes or more the loss is a fraction of one
chunk per entry and rarely matters. For a tree of very small files it is most
of the space.

The cost is the price of closed-form range addressing (Section 4.6.4): the
position of any byte of any file follows by arithmetic from the manifest,
with no scan and no index lookup, and that property depends on every payload
starting on the grid. The format does not offer a dense layout that trades
the property away. Where the trade is wanted, it is made above the format, as
described next.

### E.2. When It Matters

Two kinds of input meet the cost. The first is a subtree of very small files:
an application's cache directory, a thumbnail forest, the contents of a
decommissioned workstation archived beside the masters. The second is a file
that cannot be represented as a native entry at all: a path or symlink target
that is not valid UTF-8 (Section 4.6.6), a file type the format does not
carry (a device node, socket, or FIFO), or extended attributes outside the
portable set the caller accepts. The convention described next serves
both.

### E.3. The Wrapper Convention

The caller packs the subtree with an ordinary pax-format tar into a single
regular entry, and writes a small index beside it as a second regular entry.
For a subtree at canonical relative path `<path>`, the two entries are:

| Entry | Content |
| --- | --- |
| `<path>.remwrap.tar` | The subtree as one pax-format tar stream, produced by a tar program, with extended attributes carried in its pax records. |
| `<path>.remwrap.idx` | A JSON document describing the wrapper, with the fields below. |

The index document has three fields:

| Field | Meaning |
| --- | --- |
| `format` | The string `remanence-remwrap-idx-v1`. |
| `tar_engine` | An object recording the tar program used (`program`, `version`) and the exact `create_invocation` and `extract_invocation` argument lists, so a later reader knows what produced the wrapper and how to unpack it. |
| `entries` | An array with one element per inner entry of the wrapper, in wrapper order. |

Each element of `entries` carries:

| Field | Meaning |
| --- | --- |
| `path` | The inner entry's path, rendered as text: valid UTF-8 is kept as is, a backslash is doubled, and control bytes (0x00–0x1F, 0x7F) and bytes that are not valid UTF-8 are written as `\xNN`. An inner file whose name is not valid UTF-8 is therefore still listable. |
| `kind` | `regular`, `symlink`, `directory`, or `other`. |
| `offset` | The byte offset of the inner entry's data within the wrapper's bytes. |
| `length` | The inner entry's payload length in bytes for a regular entry; `0` otherwise. |
| `sha256` | The SHA-256 of the inner file's payload bytes, as lowercase hex. Present for regular entries; absent otherwise. |
| `mtime` | The inner entry's modification time as recorded in the wrapper's tar headers, as a string. |

Both entries are ordinary members of the object. Each begins on a chunk
boundary, each carries its own `REMANENCE.file_sha256`, and each is listed in
the manifest like any other file. The wrapper's inner files are not members:
they have no entry of their own, no `REMANENCE.file_id`, and no manifest
record. The index is a convenience of the convention rather than a
requirement of it; the caller may omit it, in which case only whole-wrapper
restore is available.

### E.4. Recovering One Inner File

A reader that understands the convention recovers a single inner file without
reading the wrapper in full:

1. Locate `<path>.remwrap.idx` and `<path>.remwrap.tar` through the manifest,
   or through a catalog derived from it.
2. Read the index entry in full and verify it against its
   `REMANENCE.file_sha256`.
3. Find the element of `entries` whose `path` is the file wanted, and confirm
   its `kind` is `regular`.
4. Read exactly `[offset, offset + length)` of the wrapper entry's payload,
   using the range addressing of Section 4.6.4 applied to the wrapper entry.
5. Verify the bytes read against the element's `sha256`.

Under the encrypted representation the same steps apply, because
range addressing is preserved through the envelope (REM-ENCRYPT §6);
the index and the wrapper range are fetched as ciphertext chunks and
decrypted, and nothing else in the object is opened. A reader that
implements the convention can offer this procedure for both
representations.

Without any of this, standard tools still suffice: extract the object with
`tar`, then extract `<path>.remwrap.tar` with `tar` again. The index remains a
readable JSON file beside it.

### E.5. What the Convention Gives Up

An inner file is not a member of the object, so nothing in Section 4 speaks
about it. It does not appear in the manifest, it has no identity of its own,
and a Verifier that checks every member has checked the wrapper and the index
as two files, not the inner files individually. Fixity for an inner file is
the `sha256` in its index element, whose own integrity rests on the index
entry's `REMANENCE.file_sha256` and, through it, on the manifest and the
object digests of Section 7. The convention defines no byte-range restore
inside an inner file, although the wrapper entry itself is range-addressable
and a reader could compose the two offsets.

### E.6. Whose Decision It Is

The choice of what to bundle into one object, and what to wrap inside it, is
the caller's policy, and it belongs above this document. Recommended practice
for deciding when to wrap is described in the REM Implementation and
Operations Guide, under “Ingest”.

## Author's Address

The ArchiveTech Project
Website: https://archivetech.org
Email: specs@archivetech.org
Reference implementation: https://github.com/archivetechie/remanence
