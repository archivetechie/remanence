# Rem Archive Object (RAO)

## Version 1 — Specification

| | |
| --- | --- |
| Status | Candidate (draft for review) |
| Revision | 10 |
| Date | 2026-06-11 |
| Format name | **Rem Archive Object (RAO)**, version 1 |
| Envelope magic | `RAO1` (encrypted representation) |
| Stream format identifier | `rao-v1` (plaintext representation, Section 4) |
| Default file extension | `.rao` |
| Reference implementation | Remanence (`crates/remanence-format`, plus the AEAD core absorbed from Amber `amber_core`) |
| Supersedes | `rem-tar-v1-candidate-specification.md` and `aof1_candidate_specification.md` — both internal candidates, never published or deployed; no compatibility constructs are carried (Section 13). [REMTAR1] remains incorporated by reference, as amended by Section 4.1, as the wire definition of the plaintext stream |

> **Drafting note (remove at freeze):** this document is the normative fixed
> point; the implementation is validated against it, not the reverse. It was
> produced by merging two candidate formats per
> `design-rem-archive-object-format.md` (the design rationale, whose §1 is
> load-bearing and not re-litigated here). Revision 2 removes every
> predecessor-compatibility construct per maintainer direction of 2026-06-11:
> neither predecessor was ever published or used in production, so the stream
> identifier becomes `rao-v1` and AOF1 read support is dropped — publicly,
> this format is version 1 (Section 13, Appendix A.13). Revision 3 applies
> external review findings: the envelope geometry is keyless-derivable, so
> keyless verification now checks the footer positionally and size leakage is
> stated exactly (Sections 5.10, 12.5); bootstrap rows for encrypted objects
> carry no manifest anchors (Appendix A.14); and internal layer numbering
> ("Layer 3c") is replaced by named components — this is a public
> specification, and the numbering is internal development shorthand.
> Revision 4 closes the second review round: catalog-less encrypted recovery
> is sequential-decrypt (no manifest anchor exists to use, Section 6.3), and
> keyless error classification is downgraded to advisory (the keyless
> geometry derivation consumes the input length itself, Section 5.10).
> Revision 5 fixes catalog-vs-bootstrap field ownership: digests live in the
> off-tape catalog, the encrypted bootstrap row carries exactly the four
> Section 8.2 envelope fields, and `object_id` comes from stored block 0
> (Sections 7.1, 12.5). Revision 6 replaces AOF1's random per-object salt
> with a derived salt (Appendix A.15): sealing consumes no randomness, is
> deterministic, and the fenced fixed-salt test interface disappears.
> Revision 7 closes the crypto review of that change: version 1 defines no
> optional envelope metadata, the metadata bytes are bound into the salt
> derivation, keyed open verifies the derivation, and the residual-collision
> claims are stated precisely (Sections 5.4.1, 5.5.3, 5.9, 12.1;
> Appendix A.16). Revisions 8–10 are errata on Revision 7's text:
> residual-claim consistency — including the SHA-256 collision branch of
> the residual model — the catalog salt check made
> deterministic-reseal-aware, and stale appendix lines. Every conflict
> resolved between the source specifications is recorded in Appendix A. Cryptographic values in the
> test vectors of Section 14 that cannot be derived by arithmetic alone are
> marked *pinned-at-generation*; producing and freezing them is a freeze
> criterion (Section 15). Open questions for the maintainer are collected in
> Appendix D.

## Abstract

This document specifies the Rem Archive Object (RAO) format, version 1: a
backend-independent byte format for large archival objects. An RAO object
bundles many named file payloads into one self-describing unit — a constrained
POSIX pax tar stream carrying per-file SHA-256 identities, closed-form
byte-range addressing, and a deterministic CBOR manifest — and exists in
exactly two representations: **plaintext**, the bare container stream,
extractable by any standard `tar`; and **encrypted**, the
same byte stream sealed inside a confidential authenticated envelope using
HKDF-SHA-256 key derivation and a chunked ChaCha20-Poly1305 stream
construction absorbed from AOF1. Both representations of one object share a
logical identity (`plaintext_digest`); each stored copy has a physical
identity (`stored_digest`) that backends scrub without keys. Encryption
preserves partial file restore: AEAD chunks coincide one-to-one with the
object's body blocks, so a per-file block index addresses ciphertext by
closed-form arithmetic. The format is designed for single-pass writing,
byte-stable fanout to tape, disk, and object storage, parity protection over
stored bytes, and long-term recovery from this document and its static test
vectors alone.

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
13. [Predecessor Formats](#13-predecessor-formats)
14. [Test Vectors](#14-test-vectors)
15. [Candidate Freeze Criteria](#15-candidate-freeze-criteria)
16. [References](#16-references)

Appendix A. [Resolved Conflicts Between the Source Specifications](#appendix-a-resolved-conflicts-between-the-source-specifications)
Appendix B. [Changes from rem-tar-v1 and AOF1](#appendix-b-changes-from-rem-tar-v1-and-aof1)
Appendix C. [Worked Example (Informative)](#appendix-c-worked-example-informative)
Appendix D. [Open Questions for the Maintainer](#appendix-d-open-questions-for-the-maintainer)

---

## 1. Introduction

### 1.1. Purpose and Design Goals

RAO wraps a set of named file payloads into one archival object. Its design
goals, in priority order:

1. **Plaintext longevity is non-negotiable.** A plaintext RAO object is a
   fully valid POSIX pax tar archive. A standard pax-aware `tar` extracts
   every payload byte-correct with no Remanence software present.
2. **Self-description.** Every object carries its own per-file index (the
   CBOR manifest): paths, sizes, SHA-256 identities, and the block address of
   every file inside the object. A catalog can be rebuilt from the medium —
   in the clear for plaintext objects, with the key for encrypted ones.
3. **Closed-form byte-range addressing (PFR).** Any byte range of any member
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
   key material and salt).
7. **Long-term recoverability.** The format is recoverable from this document
   plus its static test vectors, and — degraded, plaintext representation
   only — from knowledge of POSIX tar alone.

### 1.2. One Format, Two Representations

RAO unifies two predecessor formats:

- **`rem-tar-v1`** [REMTAR1] solved bundling, the self-describing CBOR
  manifest, partial file restore, chunk alignment, parity integration, and
  plaintext `tar`-extractability. It deliberately excluded encryption.
- **AOF1** [AOF1] solved archival encryption — an age-style STREAM
  construction with HKDF-derived per-object keys — around a weaker container
  with no per-file index.

RAO keeps rem-tar-v1's container as the one and only object body, and
absorbs AOF1's encryption construction as an optional envelope around it. The
result supersedes both: one format whose plaintext form is the rem-tar-v1
byte layout under RAO's own identifier (Section 4.1), and whose encrypted
form carries the full self-describing container confidentially. Neither
predecessor was ever published or used in production, so RAO carries no
compatibility constructs from either (Section 13, Appendix A.13); publicly,
this format is version 1. The design rationale, including why one format and
where the format-diversity boundary lies, is recorded in
`design-rem-archive-object-format.md` §1 and is not restated here.

### 1.3. Deployment Context (Informative)

In the intended deployment each archival master is kept as three copies with
different jobs:

| Copy | Role | Format |
| --- | --- | --- |
| copy-1 | working (random-access restore) | RAO, plaintext representation |
| copy-2 | offsite/DR + cloud blob | RAO, encrypted representation |
| copy-3 | shelf (cold, last resort) | plain GNU tar — **not** RAO, by design |

Copy-3 exists for format/implementation diversity and is out of scope of this
document. Copies 1 and 2 are the same RAO object in its two representations:
built once, fanned out byte-stable, sharing one `plaintext_digest`.

### 1.4. Relationship to Remanence Layers

RAO is the body format of the Remanence tape stack. This document avoids
Remanence's internal layer numbering (an implementation-team shorthand) and
names the adjacent components instead. The division of responsibility is
unchanged from [REMTAR1] §1.2:

- **RAO owns** the stored bytes of one object: tar framing, alignment, vendor
  keywords, the manifest, and (new in this document) the encryption envelope.
- **The parity layer** — the tape parity system specified by [REMPARITY],
  called "Layer 3c" in internal Remanence documents — owns everything
  outside the object's stored bytes on tape: the tape filemark terminating
  the object's tape file, parity sidecars, block-level CRCs, the BOT
  bootstrap, and the durable commit barrier. Parity is computed over
  **stored** bytes — ciphertext when the object is encrypted (Section 9).
- **The catalog and restore orchestration** above the format own catalogs,
  object selection, restore policy, and restore path sanitization.
- **The key registry is external** (Section 5.3). Objects carry only a
  16-byte `key_id`; the format never holds key material.

### 1.5. Non-Goals

RAO performs no compression (the payload workload is already-compressed
media; whole-stream compression destroys closed-form range addressing — see
[REMTAR1] §12.3 and [AOF1] §13.5). It defines no catalog format, no key
registry, no network protocol, and no multi-object container: one object is
one archive is one stored byte string. Version 1 of the plaintext stream
encodes regular files only ([REMTAR1] §1.3); the manifest reserves the
extension surfaces where a 1.x revision adds links, directories, and
ownership tiers. Re-keying an encrypted object without rewriting its payload
bytes is not supported (Section 12.8).

## 2. Conventions and Terminology

### 2.1. Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174]
when, and only when, they appear in all capitals, as shown here.

### 2.2. Conformance Roles

A single implementation may fill several roles.

- **Writer**: produces RAO objects. Comprises the **Builder** (produces the
  canonical plaintext stream; [REMTAR1] §10) and the **Sealer** (produces the
  encrypted representation; Section 5.8).
- **Planner**: computes a plaintext object's exact layout and block count
  without payload bytes ([REMTAR1] §10.2). Planning determinism extends to
  the envelope: the encrypted stored size is a closed form of the plaintext
  size (Section 5.7).
- **Reader**: recovers entries from an object in either representation
  (Section 5.9; [REMTAR1] §11).
- **Verifier**: validates a complete object end to end with key material when
  the object is encrypted (Section 7.4).
- **Keyless Verifier**: validates the public structure of an encrypted object
  and computes `stored_digest` without key material (Section 5.10).
- **Restorer**: maps payload byte ranges to stored byte ranges for partial
  file restore, in either representation (Section 6).
- **Scanner**: walks a *plaintext* object using only POSIX tar knowledge —
  the degraded 30-year fallback ([REMTAR1] §15.2). Scanners are not required
  to implement this document. No scanner role exists for the encrypted
  representation.
- **Consumer**: interprets a decoded manifest ([REMTAR1] §8.3).

### 2.3. Definitions

- **Object**: one RAO archive; the unit of write, commit, replication, and
  restore.
- **Canonical plaintext object / inner stream**: the complete plaintext
  representation byte string of an object (Section 3.1). The encrypted
  representation's AEAD payload is exactly this byte string.
- **Representation**: one of `plaintext` or `encrypted` (Section 3.2). Two
  stored copies of one object may use different representations.
- **Body block / chunk**: a fixed-size block of `chunk_size` bytes; the unit
  of alignment, addressing, and (in the encrypted representation) AEAD
  chunking.
- **`chunk_size` (C)**: the per-object body-block size. A positive multiple
  of 512; default 262144 (256 KiB). One value per object, shared by both
  representations of that object.
- **Inner `BodyLba`**: zero-based index of a body block *within the canonical
  plaintext object*. This is the address space of the manifest and of all
  catalog per-file rows, and it is identical across both representations of
  one object.
- **Stored `BodyLba`**: zero-based index of a `chunk_size` block within the
  *stored* bytes of one copy. For a plaintext copy, stored `BodyLba` equals
  inner `BodyLba`. For an encrypted copy the two spaces differ; Section 6.3
  defines the mapping.
- **Stored bytes**: the exact bytes of one stored copy, from byte 0 through
  the final byte of its final block. `stored_digest` is defined over these.
- **Envelope**: the encrypted representation's framing — plaintext header,
  metadata frame, payload frame, footer, and final fill (Section 5.1).
- **Key registry**: the external system mapping a 16-byte `key_id` to root
  key material or a retrieval procedure.
- **RAO-CBOR**: the deterministic CBOR subset of Section 5.5.1, used for the
  envelope metadata frame. **REM-CBOR**: the deterministic CBOR subset of
  [REMTAR1] §8.2, used for the manifest. The two share one deterministic
  core; their differences are inherited from the source formats and noted
  where they matter.

### 2.4. Integer, Byte, and Text Conventions

All fixed-width integers in the envelope header are unsigned, big-endian.
Byte offsets are zero-based. `KiB` = 2^10 bytes; `MiB` = 2^20 bytes.
Hexadecimal values are prefixed `0x`. All text in the format is UTF-8. All
derived quantities (offsets, frame lengths, chunk counts, block counts) are
defined over unsigned 64-bit arithmetic; implementations MUST use checked
arithmetic and MUST NOT wrap silently (Section 11).

### 2.5. Constants

| Constant | Value | Meaning |
| --- | --- | --- |
| `DEFAULT_CHUNK_SIZE` | 262144 (256 KiB) | Default body-block size |
| `STREAM_FORMAT_ID` | `rao-v1` | `REMANENCE.format_id` of the inner stream ([REMTAR1] `FORMAT_ID` as amended, Section 4.1) |
| `STREAM_SCHEMA_VERSION` | `1.0` | `REMANENCE.schema_version` of the inner stream |
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

Constants of the inner stream (`TAR_RECORD_SIZE`, `MANIFEST_PATH`,
`RESERVED_PREFIX`, pax header names, `MAX_FILE_ENTRIES`,
`MANIFEST_MAX_DEPTH`, …) are those of [REMTAR1] §2.5, unchanged.

## 3. Object Model

### 3.1. The Canonical Plaintext Object

Every RAO object has exactly one **canonical plaintext form**: the complete
stream byte string defined in Section 4 — global pax header, aligned
payload entries, manifest entry, tar EOF, and final zero fill. Its length is
always a positive exact multiple of `chunk_size` ([REMTAR1] §9.2). All
logical properties of the object — its member files, their identities, the
manifest, the per-file inner `BodyLba` index — are properties of this byte
string and are therefore identical across representations.

### 3.2. Representations

| Representation | Stored bytes | Confidentiality |
| --- | --- | --- |
| `plaintext` | The canonical plaintext object, verbatim | None; self-describing in the clear; `tar`-extractable |
| `encrypted` | The Section 5 envelope: plaintext header ‖ encrypted metadata frame ‖ chunked AEAD ciphertext of the canonical plaintext object ‖ footer ‖ zero fill | Confidential and authenticated; self-describing **with the key**; opaque without it |

There is no third representation. In particular, RAO has no analogue of
AOF1's `raw-v1` envelope: the plaintext representation is the bare container
stream itself, preserving standard-`tar` extractability (Appendix A.12).

A writer producing both copies of one object MUST derive them from the same
canonical plaintext byte string ("build once, fan out"), which is what makes
the shared identities of Section 3.3 hold.

### 3.3. Identities and Digests

| Digest | Computed over | Stored where | Verifiable without keys? |
| --- | --- | --- | --- |
| `file_sha256` | One member file's exact payload bytes | Entry pax header + manifest | Plaintext copies: yes. Encrypted copies: no |
| `manifest_sha256` | The manifest entry's CBOR bytes | Manifest pax header; for plaintext copies also the parity-layer bootstrap and catalog (A.14) | Plaintext copies: yes. Encrypted copies: no |
| `plaintext_digest` | The **complete canonical plaintext object** bytes | Encrypted copies: inside the authenticated metadata frame (Section 5.5). All copies: catalog | No (for encrypted copies) |
| `stored_digest` | The **complete stored bytes** of one copy, byte 0 through the final fill byte | External only: catalog / master index (never in-band — Appendix A.10) | **Yes** — the keyless scrub anchor |

Consequences, all normative:

1. For a plaintext copy, `stored_digest` = `plaintext_digest`. The two names
   denote the same value; catalogs MAY store it once.
2. A plaintext copy and an encrypted copy of the same object share
   `plaintext_digest` and differ in `stored_digest`. An external index joins
   copies of one logical object by `plaintext_digest`.
3. `plaintext_digest` is a function of the canonical bytes, which include the
   global header's `object_id` and `write_timestamp` keywords and the final
   zero fill. Copies share it **iff** they wrap the identical canonical byte
   string (Section 3.2). Re-*building* an object from the same input files
   with a new `object_id`, timestamp, or `chunk_size` produces a new object
   with a new `plaintext_digest`; per-file `file_sha256` values are what
   survive across rebuilds.
4. Backends MUST be able to scrub any stored copy by `stored_digest` alone,
   without keys, plaintext access, or format knowledge beyond "a byte
   string".

### 3.4. Representation Detection

For a whole-object input of unknown representation, a Reader MUST decide as
follows, examining the first bytes:

1. Bytes 0–3 equal `RAO_MAGIC` (`RAO1`) → encrypted representation
   (Section 5).
2. Bytes 0–3 equal `AOF1` → a pre-release predecessor artifact, not an RAO
   object; reject with `UnsupportedFeature` (Section 13).
3. Otherwise → attempt the plaintext representation: the input must begin
   with a valid ustar header record, typeflag `g` ([REMTAR1] §6), and its
   global header must pass the `REMANENCE.format_id = rao-v1` gate (a
   `rem-tar-v1` identifier likewise marks a pre-release artifact and fails
   the gate). A conformant plaintext object's first record is the global pax
   header, whose ustar name (`GlobalHead.0/PaxHeaders/remanence`) cannot
   collide with either magic.

This rule is for self-identification and tooling convenience; in the
Remanence deployment the catalog records each copy's representation and
readers SHOULD cross-check rather than sniff.

## 4. Plaintext Representation

### 4.1. Incorporation of the Stream Wire Format (Normative)

The plaintext representation of RAO version 1 is the byte stream specified by
[REMTAR1] — the ustar record subset, pax record grammar, global header, entry
framing and alignment, manifest encoding (REM-CBOR) and schema,
end-of-archive, writer/planner/reader/verifier obligations, error taxonomy,
and test vectors — which this document incorporates by reference in its
entirety, **as amended as follows and not otherwise**:

1. **`FORMAT_ID` = `rao-v1`.** Everywhere [REMTAR1] requires, checks, or
   pins the value `rem-tar-v1` — the §6.1 global keyword, the §6.2 reader
   gate, the §13 error conditions, the §16 vectors — the value `rao-v1` is
   substituted. The layout is otherwise byte-identical to the rem-tar-v1
   candidate; the identifier is the single wire change (Appendix A.8).
   `REMANENCE.schema_version` remains `1.0`.
2. **Successor naming.** [REMTAR1]'s references to `rem-tar-v2` as the
   breaking-change successor are read as "the RAO version 2 stream
   `format_id`" (Section 10).

No other rule is modified. In particular:

3. `REMANENCE.encryption` MUST be `none`, in every conformant stream,
   permanently. Encryption is provided exclusively by the Section 5 envelope
   *around* the stream, never flagged inside it (Appendix A.2). The keyword
   remains the refusal gate of [REMTAR1] §6.2/A.5: a reader encountering any
   other value MUST reject with `UnsupportedFeature`.
4. A standard pax-aware `tar` extracts every payload byte-correct. On
   fixed-block tape the blocking factor is `chunk_size / 512` — e.g.
   `tar -b 512 -xf /dev/nst0` for 256 KiB blocks ([REMTAR1] §15.1).

Streams carrying the retired `rem-tar-v1` identifier are pre-release
artifacts: they fail the format gate by construction and are regenerated
rather than supported (Section 13, Appendix A.13).

Sections 4.2–4.6 summarize the layout so that this document is readable on
its own and so that Section 5 and Section 6 have local anchors. Where any
summary here could be read to differ from [REMTAR1], **[REMTAR1] wins**.

### 4.2. Layout Summary

```text
+--------------------------------------------------+
| Global pax header (typeflag 'g')                 |
+--------------------------------------------------+
| Entry 0:  pax header ('x') + ustar ('0') + data  |
+--------------------------------------------------+
| ...                                              |
+--------------------------------------------------+
| Entry N-1 (last payload file)                    |
+--------------------------------------------------+
| Manifest entry ('x' + '0' + REM-CBOR data)       |
+--------------------------------------------------+
| Tar EOF: two all-zero 512-byte records           |
+--------------------------------------------------+
| Zero fill to the next chunk_size multiple        |
+--------------------------------------------------+
```

The byte stream is written as consecutive `chunk_size` body blocks; the total
length is an exact positive multiple of `chunk_size`, knowable from the file
specs alone before any payload byte is written (planning determinism,
[REMTAR1] §10.2).

### 4.3. Global Header Keywords

Exactly eight keywords, in bytewise keyword order ([REMTAR1] §6.1):
`REMANENCE.caller_object_id`, `REMANENCE.chunk_size`, `REMANENCE.encryption`
(= `none`), `REMANENCE.format_id` (= `rao-v1`),
`REMANENCE.metadata_preservation`, `REMANENCE.object_id`,
`REMANENCE.schema_version` (major 1), `REMANENCE.write_timestamp` (RFC 3339)
— with `format_id` = `rao-v1` per the Section 4.1 amendment.

### 4.4. Entries and the Alignment Rule

Every entry (payload files and the manifest) is a pax extended header +
regular ustar header + payload + 512-byte record padding, carrying `path`,
`size`, optional `mtime`, and the `REMANENCE.chunk_count`,
`REMANENCE.compression` (= `none`), optional `REMANENCE.executable`,
`REMANENCE.file_id`, `REMANENCE.file_sha256`, and `REMANENCE.pad` keywords
([REMTAR1] §7.2).

**The alignment invariant**: every entry's payload start offset `D` satisfies
`D ≡ 0 (mod chunk_size)`, achieved by sizing the `REMANENCE.pad` record of
the entry's own pax header so that

```text
O + 512 + roundup512(B) + 512 ≡ 0   (mod chunk_size)
```

where `O` is the offset of the entry's pax ustar record and `B` the pax
payload length including the pad record, with the deterministic pad-selection
rule of [REMTAR1] §7.3. Payload ends carry no padding beyond tar's normal
512-byte record padding. Chunk geometry per entry ([REMTAR1] §7.4):

```text
chunk_count     = 0 if Z = 0, else ceil(Z / chunk_size)
first_chunk_lba = absent if Z = 0, else D / chunk_size      (an inner BodyLba)
```

### 4.5. The Manifest

The final entry, path `_remanence/manifest.cbor`, is a single REM-CBOR item
([REMTAR1] §8.2: definite lengths only; unsigned integers, byte strings, text
strings, arrays, text-keyed maps, `false`/`true`/`null` only; RFC 8949
preferred serialization; map keys in bytewise order of their deterministic
*encodings*; the item fills the payload exactly). The manifest excludes
itself; its identity lives in its pax header and externally in the
parity-layer bootstrap row (plaintext copies, Section 8.2). Schema (1.0; [REMTAR1] §8.3) — top-level map, exactly these
seven text keys, shown in encoded sort order:

| Key | Type | Constraint |
| --- | --- | --- |
| `object_id` | text | = global `REMANENCE.object_id` |
| `chunk_size` | unsigned | = global `REMANENCE.chunk_size` |
| `file_entries` | array | One map per payload file, in archive order |
| `schema_version` | unsigned | = 1 |
| `object_metadata` | map | Reserved; `{}` in 1.0 writers |
| `caller_object_id` | text | = global `REMANENCE.caller_object_id` |
| `external_references` | array | Reserved; `[]` in 1.0 writers |

Each `file_entries` element — exactly these eight keys (encoded sort order):

| Key | Type | Constraint |
| --- | --- | --- |
| `path` | text | Effective entry path |
| `file_id` | text | Entry `REMANENCE.file_id` |
| `executable` | `true`/`false`/`null` | `null` when unsupplied |
| `size_bytes` | unsigned | Effective payload length |
| `chunk_count` | unsigned | Section 4.4 value |
| `file_sha256` | bytes | Exactly 32 bytes (binary; hex in pax) |
| `first_chunk_lba` | unsigned/`null` | Inner BodyLba; `null` iff `size_bytes` = 0 |
| `metadata_preservation_data` | map | Reserved; `{}` in 1.0 writers |

Consumer obligations — anchor-digest verification before interpretation,
schema validation, cross-checks against the global header, tolerance of
unknown keys and non-empty reserved containers as 1.x extensions — are those
of [REMTAR1] §8.3.

### 4.6. End of Archive

Two all-zero 512-byte records, then zero fill to the next `chunk_size`
multiple — the only block-level zero fill in the stream, tar-safe because it
lies beyond the archive EOF. `total_size_bytes` and the exact block count
follow from [REMTAR1] §9.2.

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

The header, footer, and fill are plaintext. Everything the object *says* —
the metadata frame and the payload frame (which contains the entire canonical
plaintext object, manifest included) — is encrypted and authenticated. An
encrypted RAO object reveals no filenames, file sizes, file count, or
structure (see Section 12.5 for exactly what it does reveal).

The total stored length is an exact positive multiple of `chunk_size`, as in
the plaintext representation, so that both representations map uniformly onto
fixed-size storage blocks and parity (Sections 8, 9; Appendix A.6).

### 5.2. The Plaintext Header

The header is exactly 128 bytes. Readers MUST read exactly 128 bytes and
MUST reject any input whose `header_len` field is not 128.

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

**Frozen fields.** `header_len`, `format_version`, and `suite_id` have
exactly one valid value each for the lifetime of the `RAO1` magic. They exist
for self-description and damage detection, not agility: a corrupted byte in
any of them is a hard parse error, never a silent reinterpretation. Future
revisions MUST NOT assign additional valid values; any such change requires a
new magic (`RAO2`) — Section 10.

**`object_id` field rules.** The value is the inner stream's
`REMANENCE.object_id`, encoded as 1–64 bytes of UTF-8 containing no NUL,
right-padded with NUL bytes to 64. Readers MUST strip trailing NULs to
recover the value and MUST reject an all-NUL field, a field containing an
interior NUL (a NUL byte followed by any non-NUL byte), or invalid UTF-8,
with `InvalidObjectIdField`. Consequently an object whose `object_id` exceeds
64 UTF-8 bytes cannot be stored in the encrypted representation; writers MUST
reject such input at sealing time (`InvalidInput`). (The plaintext stream
imposes no such bound; the cap is an envelope constraint. Operationally,
object identifiers are UUID strings — 36 bytes. Flagged in Appendix D.2.)

**Header validation order** (RECOMMENDED; error names per Section 11):

1. Read exactly 128 bytes (`UnexpectedEof`).
2. `magic` (`InvalidMagicBytes`) — with the Section 3.4 note that `AOF1`
   magic indicates a pre-release predecessor artifact (Section 13).
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
fault each (Section 14.3).

### 5.3. Key Identification and the Key Registry

The `key_id` field is a 16-byte opaque archive key identifier; RAO assigns no
internal semantics to it. The key registry is an external system responsible
for generating `key_id` values, mapping each to root key material or a
retrieval procedure (HSM, KMIP, escrow), and preserving key-epoch lifecycle
records. Human-readable epoch labels MUST NOT be stored in the header.

Implementations never fetch keys: callers supply root key material through an
in-memory interface (in the reference implementation, a 32-byte key file
named by `--key-file`, with the `key_id` supplied alongside). Implementations
MUST reject root key material shorter than 32 bytes (`InvalidRootKey`),
SHOULD use exactly 32 uniformly random bytes, SHOULD zeroize key material
when no longer needed, and MUST NOT write key material to logs, diagnostics,
or command lines. Only `key_id` is persisted in the object.

### 5.4. Key Derivation

AEAD mode uses HKDF with SHA-256 [RFC5869]. `HKDF(ikm, salt, info, len)`
means HKDF-Extract with `salt` and `ikm`, then HKDF-Expand with `info` to
`len` bytes. Let:

```text
header_bytes = the exact 128 bytes stored at offsets 0x00..0x7F
header_hash  = SHA-256(header_bytes)
root_key     = key material resolved via key_id   (Section 5.3)
salt         = hkdf_salt from the header
```

```text
object_secret = HKDF(ikm = root_key,      salt = salt,  info = "rao1-object-v1" || header_hash, len = 32)
metadata_key  = HKDF(ikm = object_secret, salt = empty, info = "rao1-metadata-v1",              len = 32)
payload_key   = HKDF(ikm = object_secret, salt = empty, info = "rao1-payload-v1",               len = 32)
```

This is AOF1's construction ([AOF1] §7) with the `info` labels renamed from
`aof1-*` to `rao1-*` for domain separation: an RAO object and a pre-release
AOF1 artifact sealed under the same root key can never derive the same keys,
even hypothetically (Appendix A.5). Three properties are deliberate and MUST
survive any refactoring:

1. **The header is bound through derivation, not AAD.** `header_hash` is an
   input to `object_secret`, so changing any header bit — including a
   structurally valid change such as substituting another epoch's `key_id`
   or flipping a salt bit — changes every derived key and fails all
   subsequent authentication. The AEAD AAD can therefore stay empty
   (Sections 5.5.2, 5.6.2); binding the header again as AAD would be
   redundant.
2. **Salt distinctness is derived, not assumed.** The salt is a PRF output
   binding the object's identifier, content, and metadata (Section 5.4.1),
   so distinct objects derive distinct salts — and therefore distinct
   keys — even if every environmental safeguard fails, up to the
   Section 12.1 residual (identifier reuse combined with a SHA-256 digest
   collision or a 2^−128 truncated-PRF collision). The header carries no
   payload-*recoverable*
   data: the salt binds content through a PRF under `root_key` and reveals
   nothing without it (Section 12.5).
3. **Metadata and payload keys MUST remain separate**, derived with the
   distinct labels above, never unified or derived from one another. The
   metadata zero nonce is byte-identical to a non-final payload chunk 0
   nonce; only the key separation makes that collision harmless
   (Section 12.2).

#### 5.4.1. Salt Derivation

The Sealer MUST derive `hkdf_salt` — it is never drawn from a random number
generator (Appendix A.15):

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

where `"rao1-salt-v1"` is `LABEL_SALT` (12 ASCII bytes, no terminator),
`ctr` is a single byte initially `0x00`, `object_id_field` is the exact
64-byte NUL-padded header field of Section 5.2, `plaintext_digest` is the
32-byte digest of the canonical plaintext object (known before sealing
begins, Section 5.8), and `metadata_hash` covers the serialized metadata
plaintext (serialized before salt derivation, Section 5.8 step 2; in
version 1 the metadata is itself a deterministic function of the canonical
object, Section 5.5.3 — the input exists so that *anything* the
`metadata_key` will ever encrypt is bound into the key derivation,
Appendix A.16). In the astronomically improbable case (probability 2^−128)
that the result is all zero — reserved as invalid in the header — the
Sealer MUST increment `ctr` and re-derive. The derivation is total and
deterministic: given (`root_key`, `object_id`, canonical bytes) — and the
metadata, which version 1 fixes as a function of those — the salt, and
consequently the entire sealed object, is reproducible.

Properties, each load-bearing:

1. **Distinct objects derive distinct keys.** Objects with distinct
   `object_id`s derive distinct keys through header binding alone,
   whatever their salts. Objects improperly *sharing* an `object_id` (an
   orchestrator bug; identifiers are unique by system invariant) still
   derive distinct salts whenever their `plaintext_digest` or metadata
   bytes differ — distinct content or metadata yields distinct digest
   inputs barring a SHA-256 collision — up to a 2^−128 pairwise collision of the 16-byte
   truncated PRF output, an accidental-only event, since the PRF is keyed
   by `root_key` and no party without the root key can compute, let alone
   grind, salt values (Section 12.1). No environmental condition (cloned
   VM, fork without
   reseed, broken entropy source) can cause two different objects to
   derive identical keys: sealing consumes no randomness at all.
2. **Resealing is byte-stable.** Sealing the same canonical object under the
   same epoch reproduces the byte-identical encrypted copy (identical salt,
   keys, ciphertext, `stored_digest`) — extending byte-stable fanout to
   independently produced encrypted copies, and making the Section 14
   vectors reproducible by rule. The fenced "unsafe fixed-salt" interface
   AOF1 needed for test vectors does not exist in RAO. The disclosure this
   determinism implies — that two stored copies are reseals of one object —
   is already public via the header `object_id`.
3. **The salt discloses nothing.** It is a PRF output under `root_key`:
   without the root key it reveals nothing about the content, unlike a raw
   content digest in the header would (which would enable confirmation
   attacks against guessable payloads — rejected for that reason).

The salt MUST be derived exactly as specified; a Sealer MUST NOT accept a
caller-supplied salt; and the derivation is **verified, not trusted**: every
keyed open recomputes it and rejects a mismatch (Section 5.9 step 4), so a
defective sealer cannot produce a readable object that violates this
section. Section 12.1 analyzes the failure model.

### 5.5. The Metadata Frame

#### 5.5.1. RAO-CBOR

The metadata plaintext is a single CBOR [RFC8949] data item in the
deterministic subset **RAO-CBOR**, which is defined as identical to AOF1-CBOR
([AOF1] §5.1–5.2), restated here so this document stands alone:

**Item repertoire** — an RAO-CBOR item MUST be one of:

| Major type | Permitted |
| --- | --- |
| 0 | Unsigned integers 0 through 2^64 − 1 |
| 1 | Negative integers −1 through −2^64 |
| 2 | Definite-length byte strings |
| 3 | Definite-length UTF-8 text strings |
| 4 | Definite-length arrays of RAO-CBOR items |
| 5 | Definite-length maps of RAO-CBOR key/value pairs |
| 7 | Simple values `false` (20), `true` (21), `null` (22) only |

Tags, indefinite-length items, floats of any width, `undefined`, one-byte
simple values, and all other simple values MUST NOT appear; decoders MUST
reject them (`InvalidCborEncoding`).

**Encoding requirements** (decoders MUST reject violations with
`InvalidCborEncoding`): shortest-possible integer and length encodings
(RFC 8949 preferred serialization); valid UTF-8 text; map keys in strictly
ascending bytewise order of their deterministic encodings, no duplicates; the
item occupies the metadata plaintext exactly, no trailing bytes. Canonical
form MUST be validated over the original encoded bytes — not by
decode-and-re-encode — and unknown top-level keys MUST survive validation.

**Structural limits** (format-level): nesting depth ≤
`RAO_MAX_CBOR_NESTING_DEPTH` (32), counting the top-level item as depth 1;
total item count ≤ `RAO_MAX_METADATA_ITEMS` (65536), counting the top-level
item and, recursively, every array element, map key, and map value. Writers
MUST NOT exceed them; readers MUST reject violations
(`InvalidCborEncoding`), SHOULD enforce both incrementally during decoding,
and MUST bound allocations by the declared frame length, never by counts read
from the CBOR stream.

(The manifest inside the inner stream uses REM-CBOR, [REMTAR1] §8.2 — the
same deterministic core with a narrower repertoire and text keys. One
deterministic-CBOR validator serves both; the repertoire differences are
inherited from the source formats. See Appendix A.7.)

#### 5.5.2. Metadata Encryption

The stored metadata frame is a single ChaCha20-Poly1305 [RFC8439] AEAD
ciphertext over the RAO-CBOR metadata plaintext:

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
(Section 5.4.1), so a different metadata plaintext — even for the same
object and content — yields a different salt, header hash, and
`metadata_key`. Keys repeat only when identifier, content, *and* metadata
all repeat, i.e. only when the zero nonce re-encrypts the byte-identical
plaintext, which is not nonce reuse in the harmful sense (Section 12.1). The zero nonce is
byte-identical to the nonce of a non-final payload chunk 0; the
metadata/payload key separation is what prevents that from being nonce reuse
(Section 12.2). The AAD is empty because the header is already bound through
key derivation (Section 5.4 property 1).

#### 5.5.3. Metadata Schema

The top-level metadata item MUST be an RAO-CBOR map whose keys are all
unsigned integers (`InvalidCborEncoding` otherwise).

Required keys:

| Key | Name | Type | Constraint |
| ---: | --- | --- | --- |
| `0` | `metadata_version` | unsigned | MUST be 1. |
| `1` | `plaintext_size` | unsigned | Length `P` of the canonical plaintext object in bytes. MUST be a positive exact multiple of the header `chunk_size` (Section 5.6.1). |
| `2` | `plaintext_digest_alg` | text | MUST be `sha256`. |
| `3` | `plaintext_digest` | bytes | Exactly 32 bytes: SHA-256 of the canonical plaintext object (Section 3.3). |

Missing required keys → `MissingRequiredMetadataField`; wrong type or value —
including `metadata_version` ≠ 1, a `plaintext_size` of zero or not a
multiple of `chunk_size`, a digest of the wrong length, or a
`plaintext_size` for which any Section 5.6 derived quantity overflows —
→ `InvalidMetadataField`.

Version 1 defines **no optional keys**: a conformant writer MUST emit
exactly the four required keys and nothing else. The metadata plaintext is
therefore a deterministic function of the canonical object — a property the
zero-nonce argument and reseal determinism rely on (Sections 5.4.1, 5.5.2,
12.1; Appendix A.16). The descriptive keys AOF1 defined here are redundant
in RAO: the object identifier lives in the plaintext header, the payload
type is fixed by this specification, the write timestamp lives in the
canonical global header, and application metadata belongs in the manifest's
reserved containers — inside the canonical, encrypted, digest-covered
bytes, where it cannot perturb the envelope.

Unknown top-level unsigned integer keys remain the 1.x read-side extension
surface: readers MUST NOT reject metadata for containing them, MUST NOT let
them alter parsing or interpretation, and SHOULD preserve them bytewise when
re-emitting. (A 1.x writer extension inherits zero-nonce safety
structurally, because the metadata bytes are bound into the salt
derivation — Section 5.4.1.) A defined key with the wrong type or value →
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
exactly as in AOF1 §8.2, with one parameter change: **the AEAD plaintext
chunk size is the object's `chunk_size` (`C`), not 65536.**

#### 5.6.1. Chunking

Let `P = plaintext_size`. Because `P` is a positive exact multiple of `C`
(Sections 3.1, 5.5.3):

```text
chunk_count = P / C
```

- Every plaintext chunk is **exactly `C` bytes** — there is no short final
  chunk and no empty-payload case in RAO (Appendix A.4).
- Plaintext chunk `i` is exactly inner body block `i`: the AEAD chunk
  boundaries coincide one-to-one with the canonical object's `BodyLba`
  grid. This identity is the design point that preserves partial file
  restore on ciphertext (Section 6.2; Appendix A.1).
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
and `C ≥ 512`, the counter never exceeds 2^55 and fits comfortably;
implementations MAY hold it in 64 bits, big-endian-encoded into the low 8
bytes of the 11-byte field with the high 3 bytes zero.

A reader MUST verify each chunk's tag — decrypting with the nonce determined
by the chunk's index and *computed* finality — before releasing, hashing, or
parsing that chunk's plaintext. Finality is always computed from
`chunk_count` (that is, from the authenticated `plaintext_size`), never
discovered by probing flag values or by position relative to EOF. The nonce
construction binds each chunk to its index under a key bound to this object's
header: reordered, duplicated, spliced, or cross-object chunks fail
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
the footer appears exactly at the derived `footer_offset`
(`InvalidFooter` on mismatch) and MUST NOT locate it by scanning; a byte
sequence matching the footer inside ciphertext has no meaning. The fill is
the only plaintext after the footer; Verifiers MUST confirm it is all zero
and report a nonzero fill (`FillNotZero`) — it indicates a defective writer,
damage, or a covert channel, even though it cannot affect payload recovery.
For a whole-object input, any bytes beyond `stored_size_bytes` →
`TrailingData`.

The fill is **inside** the stored bytes: it is covered by `stored_digest` and
by tape parity (Section 9), and it is what makes the stored copy an exact multiple of
`C` on every backend (byte-stable fanout, Section 8.1). This deliberately
diverges from AOF1, whose objects end at the footer (Appendix A.6).

### 5.8. Sealing

Sealing wraps a canonical plaintext object byte string. The Sealer's inputs
are: the canonical bytes (as a stream), their length `P`, their SHA-256
`plaintext_digest`, the `chunk_size` `C`, the `object_id`, the root key
material, and the `key_id`. `P` and `C` are known before sealing begins
(planning determinism); `plaintext_digest` MUST have been computed when the
canonical bytes were produced or staged — sealing is single-pass and the
digest is sealed into the metadata frame *before* the payload is written.

There are two conformant ways to obtain the input:

- **Build-and-seal**: the Builder produces the canonical bytes (Section 4 /
  [REMTAR1] §10) into a spool, file, or the plaintext copy being written in
  the same fanout, computing `P` and `plaintext_digest` as it emits them; the
  Sealer then seals from those bytes. (The build itself enforces the
  caller-supplied per-file `file_sha256` checks of Section 7.2.)
- **Seal-from-stored**: the Sealer reads an existing plaintext copy. The
  caller MUST supply the trusted catalog `plaintext_digest` (=
  `stored_digest` of the plaintext copy) as the expected value; full
  re-verification of the inner stream is then optional, because step 5 below
  proves the sealed bytes match the trusted digest.

Workflow (normative):

1. Validate inputs: `C` a positive multiple of 512; `P` a positive multiple
   of `C`; `object_id` 1–64 bytes of NUL-free UTF-8; root key ≥ 32 bytes;
   `key_id` nonzero.
2. Serialize and validate the RAO-CBOR metadata (Section 5.5), yielding `M`.
3. Derive the salt (Section 5.4.1), construct the **final** header using
   `M`, and derive keys from the hash of that final header. A Sealer MUST NOT
   derive keys from a placeholder header and backfill: a backfilled header
   silently changes `header_hash` after keys were derived, producing an
   unreadable object ([AOF1] §11.1).
4. Write the header and the metadata frame; then stream the canonical bytes
   through chunked encryption (Section 5.6), writing each stored chunk.
5. While consuming the input, independently compute the byte count and
   SHA-256 of the bytes actually read. On completion, if the computed size
   differs from `P` → fail with `PlaintextSizeMismatch`; if the computed
   digest differs from `plaintext_digest` → fail with
   `PlaintextDigestMismatch`. **On any failure the Sealer MUST NOT write the
   footer**; an object missing its footer is incomplete by definition and
   MUST NOT be treated as sealed or referenced by any durable catalog.
6. On success, write the footer and the zero fill, and report the computed
   `stored_digest` (over the complete stored bytes, fill included),
   `stored_size_bytes`, `stored_size_blocks`, and the envelope geometry
   (`M`) to the caller for cataloging.

Step 5 is mandatory even when the same component computed the digest minutes
earlier: it proves the Sealer sealed the bytes it was given, not the bytes
the metadata describes. Commit semantics are binding-specific (Section 8):
temp-file + fsync + rename for file outputs; the parity layer's
`finish_object()` for tape.

### 5.9. Opening and Verification (Keyed)

A Reader or Verifier processing a whole encrypted object MUST:

1. Read and validate the 128-byte header (Section 5.2).
2. Resolve the root key via `key_id`; derive keys (Section 5.4).
3. Read exactly `M` bytes; authenticate and decrypt the metadata frame
   (`AeadAuthenticationFailed` on tag failure); validate RAO-CBOR and the
   schema (Section 5.5.3).
4. Recompute the expected `hkdf_salt` per Section 5.4.1 — the root key, the
   header `object_id_field`, the metadata `plaintext_digest`, and the
   SHA-256 of the decrypted metadata plaintext are all now in hand — and
   reject disagreement with the header salt (`SaltDerivationMismatch`).
   This promotes the Section 5.4.1 derivation from writer policy to a
   verified property of every readable object.
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
   Reader ([REMTAR1] §11), which applies all of its acceptance rules — in
   particular the global-header gates — plus these envelope cross-checks
   (`InnerObjectMismatch` on failure):
   - inner `REMANENCE.object_id` = header `object_id`;
   - inner `REMANENCE.chunk_size` = header `chunk_size`;
   - inner `REMANENCE.format_id` = `rao-v1` and `REMANENCE.encryption` =
     `none` (these are [REMTAR1] §6.2 gates as amended; their failure inside
     a well-authenticated envelope indicates a defective sealer).

Steps 6–9 MAY be pipelined: chunk plaintext can flow into the inner Reader as
chunks authenticate. Every byte released downstream has been authenticated at
chunk granularity, so a failure mid-stream leaves the consumer with an
authentic prefix of the true plaintext — but consumers MUST NOT treat
streamed output as complete or valid until the open reports overall success,
and restore-mode per-file digest verification ([REMTAR1] §11.2 step 5)
applies to the inner entries as always.

**Fail-closed rule.** On any AEAD tag failure, the implementation MUST stop,
MUST NOT release that chunk's plaintext, and MUST NOT retry with different
parameters (other finality, other index, other key). There is no salvage mode
across a failed tag: recovery of a damaged encrypted object is the parity
layer's job on the stored bytes (Section 9), after which decryption is
retried on the recovered ciphertext. ([REMTAR1]'s salvage mode remains available for the
*decrypted inner stream* — authenticated bytes whose inner structure is
damaged indicate a defective writer, and salvaging them is a deliberate,
explicitly-labeled reader mode as ever.)

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
  `roundup(footer_offset + 16, C) ≠ S` — then verify the footer bytes at
  the derived `footer_offset` (`InvalidFooter`) and the all-zero fill
  (`FillNotZero`). **Keyless error classification is advisory**: the
  derivation consumes the input length itself, so truncated and appended
  bytes shift the derived `chunk_count` rather than presenting as surplus.
  (Appending one all-zero block to RAO-TV-E1 derives a self-consistent
  6-chunk geometry whose footer check then fails at the wrong offset —
  `InvalidFooter`, not `TrailingData`; appending 306 bytes shifts the
  derived count from 5 to 6 and is caught only as a non-block-multiple
  length.) A keyless verifier MAY report `UnexpectedEof` or
  `TrailingData` for any inconsistent length; the exact Section 5.7
  classification belongs to keyed readers, which learn the true
  `plaintext_size` from authenticated metadata, and to verifiers holding a
  catalog-recorded stored length or `stored_digest`;
- MUST NOT claim authentication: a deliberately extended input can present
  a self-consistent geometry, a well-placed footer, and clean fill — the
  derived geometry and footer prove framing and completeness only, and the
  computed `stored_digest` is meaningful solely against a trusted catalog
  value.

Keyed readers MAY cross-check the derived `chunk_count` against the
decrypted `plaintext_size`; a mismatch in a tag-valid object is impossible
by construction, so a disagreement indicates an arithmetic defect, not
damage.

Keyless **inspect** (the operational `rem archive inspect` surface) reveals
exactly the header fields: magic, version, suite, `chunk_size`, `key_id`,
`hkdf_salt`, `metadata_frame_len`, `object_id` — plus the stored length and the derived
`plaintext_size`/`chunk_count` above. It reveals **no** filenames, member
sizes, file count, or manifest content. Inspect is how an operator recovers
*which* key epoch to materialize (`key_id` → registry) without holding any
key.

## 6. Partial File Restore

PFR maps a member-file byte range to stored byte ranges by closed-form
arithmetic. The per-file index — `first_chunk_lba` (an inner `BodyLba`) and
`size_bytes` per file, from the manifest or the catalog — is the **same for
both representations** of an object, because both wrap the same canonical
bytes. Catalog per-file rows therefore need to be stored once per object, not
per copy.

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
`x mod C` (file payloads start block-aligned, Section 4.4). The requested
bytes are obtained from inner blocks `b_first ..= b_last` with head/tail
trimming; the final block of a file holds `Z − (chunk_count − 1) × C` payload
bytes, with unrelated stream bytes after them ([REMTAR1] §7.4) — trim by
`Z`, never by block boundaries.

For a **plaintext copy** this is the whole computation: inner blocks are
stored blocks; read them and trim.

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
therefore requires the key by construction, plus one metadata-frame read —
there is no per-file table to fetch and no manifest read on the restore path
(the per-file row comes from the catalog; with no catalog, recovery of an
encrypted copy decrypts the inner stream sequentially until the manifest —
the final entry — is parsed, because encrypted bootstrap rows deliberately
carry no manifest anchor (Section 8.2, Appendix A.14); PFR against the
freshly recovered manifest then proceeds by this same mapping).

PFR indexes MUST treat plaintext offsets (inner `BodyLba`, file byte ranges)
as the source of truth and MUST NOT make ciphertext offsets canonical; stored
offsets are derived, reproducible from this section ([AOF1] §13.1 rule,
retained).

### 6.4. Stored-Block Mapping (Tape and Block-Addressed Backends)

On a byte-addressed backend (file, object store with range reads), the
Section 6.3 stored byte range is fetched directly. On tape, the stored copy
is one tape file of fixed blocks of size `C` (Section 8.2), addressed by
stored `BodyLba`. A stored byte range `[a, a + l)` maps to stored blocks:

```text
first_stored_block = floor(a / C)
last_stored_block  = floor((a + l − 1) / C)
```

Because each stored chunk is `C + 16` bytes, the ciphertext of one inner
block spans at most `ceil((C + 16) / C) + 1 = 3` consecutive stored blocks,
and a run of `k` consecutive chunks occupies one contiguous stored-block
range of at most `k + 2` blocks (with a 64-bit `b` and 16-byte tags, the slip
is 16 bytes per chunk — the +16 skew is why stored `BodyLba` ≠ inner
`BodyLba` for encrypted copies). This bounded, contiguous read amplification
is the accepted cost of keeping the stored stream block-uniform; the
alternative (sizing AEAD chunks to `C − 16` so ciphertext lands
block-aligned) was rejected — Appendix A.1 records the trade.

## 7. Digests, Integrity, and the Verification Chain

### 7.1. The Chain of Trust

```text
off-tape catalog                      on-tape parity-layer bootstrap row
(stored_digest + plaintext_digest     (plaintext copies: manifest location +
 per copy — Section 12.5 trust         manifest_sha256; encrypted copies:
 domain)                               the Section 8.2 envelope fields only — A.14)
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
per-file  file_sha256, size_bytes, first_chunk_lba, chunk_count
        │
        ▼
payload bytes ── byte-verified by file_sha256
```

The pax `REMANENCE.file_sha256` keywords duplicate the manifest hashes as a
within-stream cross-check. For an encrypted copy, the decrypted manifest's
anchor is the authenticated `plaintext_digest`, which covers every canonical
byte including the manifest; the [REMTAR1] §8.3 anchor-digest obligation is
satisfied by it (with the manifest entry's own pax `REMANENCE.file_sha256`
as the within-stream self-consistency check) — external manifest anchors
exist for plaintext copies only (Appendix A.14). What the chain does **not**
provide is covered in Sections 12.6 (no self-authentication of plaintext
copies) and 12.7 (non-committing AEAD).

### 7.2. Write-Path Verification (No Extra Reads)

Every digest in the chain is computed over bytes already flowing through the
writer — the chain costs hash arithmetic, never an additional read pass:

1. **Per-file, at build.** The Builder streams each payload file, hashing it,
   and MUST fail the object if the streamed SHA-256 or byte count differs
   from the caller-supplied expected `file_sha256`/size ([REMTAR1] §7.5,
   §10.1). This proves the writer archived the payload it was given, not the
   payload the metadata describes. A failed object MUST NOT be completed or
   reported as complete.
2. **Canonical stream, at build.** The Builder computes `plaintext_digest`
   (= the plaintext copy's `stored_digest`) over its own emitted byte
   stream, and reports it with the layout for cataloging.
3. **Envelope, at seal.** The Sealer recomputes size and digest of the bytes
   actually sealed and fails — footer unwritten — on mismatch
   (Section 5.8 step 5). It computes the encrypted copy's `stored_digest`
   over its own emitted bytes.

### 7.3. Post-Write Re-Verification (Deployment Obligation)

After each copy is written, and before that copy is recorded durable, the
deployment MUST re-read the copy via the object read path and re-verify it —
for a full verification, every member `file_sha256` (which transitively
exercises the envelope on encrypted copies); at minimum, the copy's
`stored_digest`. This is a media/transmission guard, deliberately distinct
from the Section 7.2 build checks: it is the one intentional extra read in
the pipeline, and it is a *deployment* (workflow) obligation rather than a
property of the bytes — a conformant Verifier (Section 7.4) is the tool that
discharges it.

### 7.4. Verifier Profile

A Verifier validates one stored copy end to end without extracting it:

- **Plaintext copy**: the five obligations of [REMTAR1] §11.4 — full restore-
  mode read with every entry digest checked, manifest anchor-digest and
  schema validation, manifest-vs-archive correspondence, final-fill zero
  check, report-all-nonconformities — plus a `stored_digest` comparison
  against the catalog value when available.
- **Encrypted copy (keyed)**: Section 5.9 in full (header, metadata, salt
  derivation, every chunk tag, plaintext size+digest, footer, fill, inner
  cross-checks), with
  the recovered inner stream verified per [REMTAR1] §11.4, plus the
  `stored_digest` comparison.
- **Encrypted copy (keyless)**: Section 5.10 — structure plus
  `stored_digest`; explicitly not an authentication claim.

A successful keyed verification means: every payload byte hashes to its
declared identity, the object's self-description is complete and consistent,
and (encrypted) every stored chunk authenticates under the object's derived
keys.

### 7.5. Scrub

Backends scrub stored copies by `stored_digest` (whole-copy) without keys.
On tape, the parity layer additionally CRCs every stored block and can verify and
repair at block granularity without reading the whole object (Section 9).
Both operate on stored bytes and are representation-agnostic.

## 8. Storage Bindings and Backend Independence

### 8.1. The Byte-Format Contract

An RAO object in either representation is a byte string. Any conformant tool
can produce it; any backend can store it; `stored_digest` is computed over
the identical bytes everywhere ("byte-stable fanout"). A backend needs no
keys, no plaintext access, and no format knowledge to store, replicate,
compare, or scrub a copy. Backends SHOULD record per copy: location,
representation, `stored_digest`, `stored_size_bytes`/block count,
`chunk_size`, and — for encrypted copies — `key_id` and
`metadata_frame_len` (the latter so PFR arithmetic, Section 6.3, needs no
header read).

### 8.2. Tape Binding

The stored bytes are written as one tape file of fixed-size tape blocks,
terminated by a filemark written by the parity layer at object close. The tape block
size MUST equal the object's `chunk_size`, for both representations — one
stored block is one tape block, stored `BodyLba` is the tape file's block
index, and parity geometry is uniform. (For the plaintext representation this
restates [REMTAR1] §3.2; extending it to encrypted copies is a decision of
this document, Appendix A.6.) Parity sidecars, the filemark map, block CRCs,
and the BOT bootstrap are tape-binding artifacts owned by the parity layer
(Section 9); they exist **only** on tape and are not part of the object's
stored bytes on any backend.

Bootstrap rows differ by representation, deliberately (Appendix A.14). A
**plaintext** object's row carries the manifest anchors of [REMTAR1] §8.1
(`manifest_first_chunk_lba` — an inner `BodyLba` — `manifest_size_bytes`,
`manifest_chunk_count`, `manifest_sha256`). An **encrypted** object's row
MUST NOT carry manifest anchors: it carries only the envelope fields of
Section 8.1 (representation, `key_id`, `metadata_frame_len`, stored block
count). Manifest size and location are structural facts about confidential
content — manifest size correlates directly with member count — and the
bootstrap is plaintext on the very tape the envelope protects; the envelope's
authenticated metadata already anchors the manifest more strongly than an
external digest could (Section 7.1). Catalog-less recovery from tape:
keyless, an operator recovers each encrypted object's `object_id` and
`key_id` from stored block 0 (the header); with the key, the full
self-describing object — the manifest located by sequential decryption
rather than by anchor, an acceptable cost on a recovery path over sequential
media. Plaintext objects remain fully recoverable keyless ([REMTAR1] §15).

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
map, and the replicated BOT bootstrap. **The parity construction and
geometry are unchanged by this document**; what RAO adds is bootstrap
metadata (item 4) and one clarification now load-bearing (item 1):

1. **Parity is computed over stored bytes** — the ciphertext, when the copy
   is encrypted. The order is: build → (seal) → parity. The parity layer
   protects bytes regardless of content and needs no keys, ever; recovery of damaged blocks
   of an encrypted object proceeds keyless, after which decryption is
   retried on the recovered stored bytes (Section 5.9 fail-closed rule).
2. Within one object's tape file there are no parity or bootstrap blocks; the
   object's stored blocks are contiguous (stored `BodyLba` 0..N−1). Parity
   epochs span objects via `ParityDataOrdinal`; sidecars land between tape
   files. None of this is visible in, or part of, the object's stored bytes.
3. An object is *committed* when the parity layer's `finish_object()`
   returns; neither
   representation defines an in-band commit marker (the envelope footer
   detects incomplete writes; it is not a commit barrier).
4. The bootstrap row extension for encrypted objects (representation marker,
   `key_id`, `metadata_frame_len`, stored block count — and never manifest
   anchors, Section 8.2) is a parity-layer schema addition, not a parity
   change; tracked in Appendix D item 6.

## 10. Versioning and Extensibility

RAO version 1 is identified by the **pair** of wire identifiers:

- **Plaintext stream**: `REMANENCE.format_id = rao-v1`,
  `REMANENCE.schema_version = 1.<minor>`. Evolution rules are [REMTAR1] §12:
  minor revisions are additive (new pax keywords, new manifest keys, content
  in the reserved containers) and MUST NOT change any rule a 1.0 reader
  enforces; any change that could make a 1.0 reader misinterpret bytes
  requires a new `format_id` — which is, by definition, RAO version 2.
- **Envelope**: magic `RAO1`, `format_version` 1, `suite_id 0x01` — all
  frozen (Section 5.2). Any change to envelope parsing, cryptography, chunk
  framing, or metadata semantics requires a new magic (`RAO2`). There is no
  in-place agility: no negotiable suites, no version ranges. This is a
  deliberate trade — every valid `RAO1` envelope is parseable by every
  conformant reader forever. New *metadata* keys are the one extension
  surface (Section 5.5.3), and they MUST remain descriptive.

The two identifiers version together: a future plaintext-stream break and a
future envelope break each produce RAO version 2; this document's successor
would then specify the new pair. `REMANENCE.encryption` remains `none` in
every conformant stream of every 1.x revision; introducing any other value is
likewise a version-2 change (Appendix A.2).

## 11. Errors

Implementations SHOULD expose typed errors equivalent to the taxonomy below.
Names are normative for the test-vector manifests (Section 14); surface
syntax is not. Inner-stream errors are those of [REMTAR1] §13, unchanged, and
apply to plaintext copies and to the decrypted inner stream of encrypted
copies alike.

Envelope errors (encrypted representation):

```text
InvalidMagicBytes            input does not begin with RAO1 (an AOF1 magic marks a
                             pre-release predecessor artifact, Section 13)
InvalidHeaderLength          header_len field is not 128
UnsupportedFormatVersion     format_version is not 1
InvalidSuite                 suite_id is not 0x01
InvalidChunkSize             chunk_size is zero or not a multiple of 512
ReservedBytesNotZero         flags or reserved bytes are nonzero
InvalidKeyIdentifier         all-zero key_id, or key_id unknown to the resolver
InvalidSalt                  all-zero hkdf_salt
SaltDerivationMismatch       header hkdf_salt differs from the Section 5.4.1 derivation
                             (keyed open/verify, Section 5.9 step 4)
InvalidObjectIdField         object_id field empty, interior NUL, or invalid UTF-8
MetadataFrameLengthInvalid   metadata_frame_len outside [17, 16 MiB]
InvalidRootKey               root key material shorter than 32 bytes
UnexpectedEof                declared header, metadata frame, footer, or fill bytes missing
MissingFinalChunk            EOF within the payload frame before an authenticated final chunk
AeadAuthenticationFailed     metadata or chunk tag verification failed
InvalidCborEncoding          metadata frame plaintext is not valid RAO-CBOR (Section 5.5.1)
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

Single-fault condition mapping (normative for negative test vectors): each
error above maps from exactly the condition described beside it; the
recommended detection orders are Section 5.2 (header) and Section 5.9
(whole object). For multi-fault inputs any applicable error is conformant.
I/O failures MUST remain distinguishable from format violations. No code
path reachable from object bytes may panic, crash, or allocate unboundedly
(Section 12.9).

## 12. Security Considerations

### 12.1. Per-Object Key Uniqueness Is Structural

The catastrophic failure of this construction would be two objects under one
root key deriving identical `metadata_key`/`payload_key` for *different*
plaintexts: the zero metadata nonce and every payload chunk-index nonce
would then be reused across distinct contents — for ChaCha20-Poly1305 a
total failure (XOR of plaintexts leaks immediately; Poly1305 key recovery
enables forgery). RAO closes every operational path to that state by two
independent mechanisms, leaving only the quantified residual stated below:

1. **Header binding.** Keys derive from `header_hash`, and the header
   contains the 64-byte `object_id`: objects with distinct identifiers
   derive distinct keys *regardless of their salts*. (This is already
   stronger than AOF1, whose header carried no object identifier and whose
   security therefore rested on salt freshness alone.)
2. **Salt derivation.** The salt is a PRF of the object's identifier, its
   content digest, *and* its metadata bytes (Section 5.4.1): objects sharing
   an identifier but differing in content or metadata derive distinct salts,
   hence distinct headers, hence distinct keys — and the derivation is
   re-verified on every keyed open (Section 5.9 step 4), so a defective
   sealer cannot produce a readable object that violates it.

Every residual path to identical keys over distinct plaintexts begins with
an `object_id` reused across seals that differ in content or in envelope
metadata (an orchestrator or sealer bug — identifiers are unique by system
invariant; v1 metadata cannot differ for one object, Section 5.5.3, but a
1.x metadata extension can, which is why the model covers it); for distinct
identifiers, header binding ends the analysis. Given such a reuse, keys
collide only if one of two further things happens:

1. **A SHA-256 collision in the digest inputs.** The derivation sees
   content only through `plaintext_digest` and metadata only through
   `metadata_hash`, so two distinct same-size canonical objects with
   colliding content digests — or two distinct metadata plaintexts with
   colliding hashes — would derive identical salts, headers, and keys
   outright. This is a collision-resistance break of SHA-256 — an
   assumption the format already stakes its entire integrity chain on
   (`file_sha256`, `plaintext_digest`, `manifest_sha256`; Section 7.1) —
   so the residual model adds no primitive assumption the format did not
   already carry.
2. **A truncated-PRF output collision.** Distinct derivation inputs can
   still collide in the 16-byte salt with probability 2^−128 per pair — a
   truncation bound, not a SHA-256 or HMAC break (128 bits of salt cannot
   promise more), and accidental-only: the PRF is keyed by `root_key`, so
   no party without the root key can compute — let alone grind for — salt
   values, and a party holding the root key is already inside the trust
   boundary (Section 12.7).

Sealing consumes no randomness, so RNG quality *during
sealing* is not a confidentiality dependency; the root key itself, generated
once in the external registry, still requires high-quality randomness
(Section 5.3) — that is where the format's reliance on entropy begins and
ends. What *can* recur is the benign case: resealing the identical canonical
object (identical metadata included — in version 1 the metadata is a
function of the object, Section 5.5.3) reproduces identical keys with the
identical plaintexts — deterministic encryption of one object, disclosing
only an equality that the header `object_id` already discloses
(Section 5.4.1 property 2).

On the key-dependent Extract salt (a salt that is a PRF output under the
same `root_key` later used as the Extract `ikm`): TLS 1.3's key schedule
normalized chaining key-derived values as HKDF-Extract salts, but RAO's
shape is not identical to TLS 1.3's, and no proof is inherited from it. The
construction rests explicitly on the dual-PRF assumption for HMAC-SHA-256 —
that it behaves as a PRF when keyed through either input — the same
assumption underlying the TLS 1.3 analyses. It is stated here as an
assumption, deliberately.

As a deployment belt, the catalog SHOULD run a consistency check on
`(key_id, hkdf_salt)` at insert. A repeat is *legitimate* exactly when the
rows are byte-identical copies of one sealed object — agreeing on
`object_id`, `plaintext_digest`, **and** `stored_digest`: byte-stable
fanout and resealing intentionally produce such repeats (Section 5.4.1
property 2). A repeat disagreeing on any of the three SHOULD be rejected
loudly. The `stored_digest` term is not redundant: rows agreeing on
`object_id` and `plaintext_digest` but differing in `stored_digest` are
precisely the signature of residual branch 1 above (or of a defective
sealer) — the one case in which `plaintext_digest` equality cannot be
trusted to mean content equality. Detection rather than prevention, but
loud where silence would be dangerous.

### 12.2. Key Separation Is Load-Bearing

The metadata nonce (12 zero bytes) is byte-identical to the nonce of a
non-final payload chunk 0. The construction is safe only because
`metadata_key` ≠ `payload_key`. Any change that unifies the keys converts
this coincidence into real nonce reuse. No future revision may "simplify"
the key schedule by merging them.

### 12.3. Binding Without AAD

Both AEADs use empty AAD. The bindings the work-order checklist demands —
object identity and chunk position — are achieved structurally: the object
(including its `object_id`, `key_id`, salt, and geometry) is bound through
`header_hash` in the key derivation, and the chunk index and finality are
bound through the nonce. Cross-object splicing fails because the keys
differ; intra-object reordering, duplication, or truncation fails because
the nonce (index, finality) or the missing final chunk fails authentication.
Re-binding the same facts as AAD would add bytes and a second mechanism
without adding security.

### 12.4. Fail-Closed

A failed chunk or metadata tag MUST stop processing without releasing that
chunk's plaintext (Sections 5.9, 6.3); a failed seal MUST NOT produce a
footer (Section 5.8); a parity or CRC failure on stored blocks is repaired by
the parity layer before decryption is retried. Partial plaintext is never emitted as
success; streamed output is not valid until the whole-object open succeeds.

### 12.5. Confidentiality Boundary and Size Leakage

An encrypted copy reveals, by design: that it is an RAO object; the suite;
`chunk_size`; `key_id` (the key *epoch*, not the key); the salt;
`metadata_frame_len`; `object_id`; and its stored length — from which the
**exact** `plaintext_size` and `chunk_count` are derivable by the
Section 5.10 arithmetic. Encryption hides content, never size. It reveals no
filenames, member sizes or count, manifest content, or payload bytes. Deployments for which the object's existence, identifier, or
approximate size is itself sensitive must handle that above the format
(padding payloads before building, opaque object naming); RAO defines no
padding mechanism.

Two adjacent systems hold facts about encrypted objects in the clear and are
separate trust domains, out of scope of the format but named so nobody is
surprised. The off-tape **catalog** holds paths, per-file rows, and digests —
cleartext metadata about confidential content, protected by the catalog
system, not by this format. The on-tape **parity-layer bootstrap** is
deliberately starved: for encrypted objects it carries only the envelope
fields of Section 8.2 (representation, `key_id`, `metadata_frame_len`,
stored block count) and MUST NOT carry manifest anchors (Appendix A.14) —
`object_id` is recovered from the envelope header in stored block 0, not
from the bootstrap — so the tape itself leaks nothing beyond the
Section 5.10 header-and-size facts.

### 12.6. Plaintext Copies Are Not Self-Authenticating

A plaintext RAO object provides integrity plumbing, not authentication: an
attacker who can rewrite the medium can rewrite payloads, pax hashes, and the
manifest consistently ([REMTAR1] §14.1). The trust anchor is external — the
catalog's `stored_digest` and, on tape, the bootstrap's `manifest_sha256`
(plaintext rows, Section 8.2). Encrypted copies
are cryptographically authenticated under the root key — subject to
Section 12.7.

### 12.7. Non-Committing AEAD

ChaCha20-Poly1305 is not key-committing: a party holding two root keys can in
principle craft one ciphertext that authenticates and decrypts to different
valid plaintexts under each [AEAD-COMMIT] [PART-ORACLE]. `stored_digest` does
not prevent this (equivocation uses one byte string). RAO's defense is
operational: the key registry is trusted, `key_id` resolution is exact, and
writers are inside the trust boundary. Deployments with mutually distrusting
writers, or a requirement that no object be interpretable under two epochs,
need a committing construction and therefore RAO version 2. Note also that
the Section 5.9 step 7 digest check detects the related key-holder forgery
(authenticated metadata misstating the digest).

### 12.8. Key Rotation and Epoch Longevity

Keys are derived per object; no wrapped DEKs are stored. Rotating the root
key affects newly sealed objects only; re-keying an existing object means
resealing its canonical bytes (cheap if a plaintext copy exists — a new seal,
not a rebuild; the object and its `plaintext_digest` are unchanged). Old key
epochs MUST remain recoverable in the registry for as long as objects sealed
under them must remain readable.

### 12.9. Hostile-Input Posture

Stored bytes come off removable media and networks and MUST be treated as
untrusted in both representations. Every parse decision is bounded: the
header is fixed-size with frozen fields; the metadata frame is length-bounded
(16 MiB) with CBOR depth/item limits enforced incrementally; payload
processing uses constant memory per chunk; all arithmetic is checked; inner
stream parsing inherits [REMTAR1] §14.2–14.3 (checksummed headers, bounded
pax records, semi-trusted catalog inputs, fallible allocation). Reader
implementations MUST NOT panic, crash, or invoke undefined behavior on any
byte sequence, SHOULD enforce this mechanically (no `unwrap`/unchecked
indexing/unchecked arithmetic on reachable paths; forbid `unsafe` where
practical), and SHOULD validate it with coverage-guided fuzzing of the header
parser, both CBOR decoders, the record loop, and whole-object open/verify
(Section 15).

### 12.10. Path Traversal

Unchanged from [REMTAR1] §14.4: native paths are canonical-relative by
format rule, readers reject violations, and restore sinks keep their own
sanitization as defense in depth (they also serve foreign formats and salvage
reads). Decrypting an envelope grants no exemption: the inner stream is
parsed under the same rules.

## 13. Predecessor Formats

RAO version 1 supersedes two internal candidate formats: **rem-tar-v1** (the
container, [REMTAR1]) and **AOF1 / amber** (the encryption construction,
[AOF1]). Neither was ever published or used in production. Both are
therefore retired **without compatibility machinery** — RAO is version 1 of
the public format, and the internal candidates are not part of its version
history (maintainer decision of record, Appendix A.13):

1. No conformant RAO implementation interprets predecessor objects. An
   `AOF1`-magic input fails Section 3.4 (`UnsupportedFeature`); a stream
   carrying `REMANENCE.format_id = rem-tar-v1` fails the Section 4.1 format
   gate. These are deliberate refusals, not gaps: the gates that exist to
   protect future readers also fence off pre-release artifacts.
2. Pre-release artifacts (development tapes, harness-scenario objects,
   `.aof` files) are regenerated as RAO objects, not supported. No AOF1
   reader is ported into Remanence.
3. Amber retires as a project. Its AEAD core is absorbed as the envelope
   implementation, instantiated with the RAO parameters of Section 5
   (`rao1-*` labels, body-block chunking); nothing else of it survives.
4. The predecessor candidate specifications remain in `docs/` as historical
   references. [REMTAR1] alone retains normative force, exactly as amended
   by Section 4.1.

## 14. Test Vectors

Static vectors live in the repository (`fixtures/rao/`), each with a manifest
entry recording inputs, the expected values pinned below, and — for negative
vectors — the expected Section 11 (or [REMTAR1] §13) error name. The
[REMTAR1] §16 vector suite remains required for the plaintext stream and is
not duplicated here; the vectors below are the unified-format additions: a
plaintext object, its encrypted twin (the `plaintext_digest`-sharing pair),
and a default-chunk-size object.

Values below marked **[pinned-at-generation]** are produced by the reference
implementation when the fixtures are first generated, then frozen; they
cannot be derived by arithmetic alone and this document does not guess them.
Generating and freezing them — and confirming the *derivable* expected
values stated here — is freeze criterion 2 (Section 15). All other values
below are normative now. Payload digests are independently checkable with
`sha256sum`.

### 14.1. RAO-TV-P1 — Plaintext Object

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

Expected layout (derivable; the full derivation is worked in Appendix C):

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

### 14.2. RAO-TV-E1 — Encrypted Twin of RAO-TV-P1

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
| Metadata plaintext | 50-byte RAO-CBOR map `{0: 1, 1: 20480, 2: "sha256", 3: <32 digest bytes>}` |
| `metadata_frame_len` `M` | 66 |
| Payload frame | bytes 194–20753 (5 chunks × 4112) |
| `footer_offset` | 20754; footer + 3806 zero-fill bytes |
| `stored_size_bytes` / blocks | 24576 / 6 |

Pinned outputs **[pinned-at-generation]**: the derived `hkdf_salt`
(Section 5.4.1); the exact 128 header bytes and `header_hash`; derived
`metadata_key` and `payload_key`; the exact metadata frame bytes; SHA-256 of
the payload frame; `stored_digest`. Because sealing is deterministic
(Section 5.4.1 property 2), the entire stored byte string is reproducible
from the inputs above with no test-only interfaces. Required
equality: this object's `plaintext_digest` MUST equal RAO-TV-P1's
`stored_digest`, and the value of metadata map key `3` (`plaintext_digest`,
Section 5.5.3) MUST equal that digest.

### 14.3. RAO-TV-D1 — Default Chunk Size

One vector MUST use `DEFAULT_CHUNK_SIZE`. Inputs: `chunk_size` 262144;
`object_id` `00000000-0000-4000-8000-000000000002`; `caller_object_id`
`rao-tv-d1`; `write_timestamp` `2026-01-01T00:00:00Z`;
`metadata_preservation` `minimal`; `manifest_file_id`
`00000000-0000-4000-8000-0000000000fe`; one file `v.bin`, `file_id`
`00000000-0000-4000-8000-000000000012`, contents 262145 bytes with byte
`i` = `i mod 256` (expected `file_sha256`
`c35991ad254f48ff8b02becb9f0cc56581e86a0b477b13e5ebb0030a3b91c848`,
`chunk_count` 2), sealed both plaintext and encrypted under the RAO-TV-E1
key material (salt derived per Section 5.4.1). Pinned
outputs as in 14.1/14.2 (digests only for the large streams; exact bytes for
header, metadata frame, and manifest).

### 14.4. Negative Vectors

Each contains exactly one fault and asserts the mapped error. The suite MUST
include at least — envelope header: wrong magic; `header_len` ≠ 128;
`format_version` 2; unknown `suite_id`; `chunk_size` 0 and `chunk_size` not
a multiple of 512; nonzero `flags`; nonzero `reserved`; all-zero `key_id`;
all-zero `hkdf_salt`; all-NUL `object_id` field; interior-NUL `object_id`;
non-UTF-8 `object_id`; `metadata_frame_len` 16 and `metadata_frame_len`
> 16 MiB. Cryptographic binding: a flipped salt bit (structurally valid →
`AeadAuthenticationFailed`); `key_id` swapped to another known test key
(`AeadAuthenticationFailed`); a flipped ciphertext bit in chunk 1; chunks 1
and 2 transposed; wrong final flag (a 6th chunk appended / final chunk
re-sealed non-final); sealed metadata deliberately misstating
`plaintext_digest` (opens MUST fail `PlaintextDigestMismatch`). Metadata:
each RAO-CBOR repertoire violation (float, tag, indefinite length, duplicate
key, non-shortest encoding); missing key 1; `metadata_version` 2;
`plaintext_size` not a multiple of `chunk_size`; `plaintext_size` 0;
overflow-implying `plaintext_size`. Framing: EOF inside the metadata frame;
EOF mid-chunk; payload absent after metadata (`MissingFinalChunk`); footer
bytes wrong at the correct offset; one nonzero fill byte (`FillNotZero`);
bytes appended past the fill (`TrailingData` from keyed open/verify; the
keyless classification of the same input is advisory — Section 5.10). Inner cross-checks (sealed
with a defective-sealer test harness): inner `object_id` differing from the
header; inner `chunk_size` differing; inner `REMANENCE.encryption` ≠ `none`
(`InnerObjectMismatch`); an object sealed under an arbitrary (non-derived)
header salt, otherwise self-consistent — keyed open MUST fail
`SaltDerivationMismatch`. Writer-side: sealing input with `P` not a multiple
of `C`; `object_id` > 64 bytes; root key of 16 bytes (`InvalidRootKey`).

## 15. Candidate Freeze Criteria

RAO version 1 remains a candidate until all of the following hold; after
freeze, no normative change is permitted other than errata that do not change
the set of valid objects (anything else is RAO version 2):

1. The reference implementation implements this document — including the
   envelope and the absorbed AEAD core with `rao1-*` labels and
   parameterized chunk size — and every [REMTAR1] Appendix B
   conformance-backlog item is closed or consciously re-specified.
2. The Section 14 fixtures exist in-repo with every
   **[pinned-at-generation]** value generated, frozen, and passing, and the
   stated derivable values confirmed byte-exact; plus the [REMTAR1] §16
   suite.
3. The plaintext-interop gates pass: GNU tar, bsdtar, and Python `tarfile`
   extract every positive plaintext vector byte-identically
   ([REMTAR1] §15.3), including RAO-TV-P1 and the plaintext RAO-TV-D1.
4. The `plaintext_digest` equality of Section 14.2 is demonstrated end to
   end: one build, two representations, shared logical identity, differing
   `stored_digest`.
5. PFR on ciphertext is tested by actually fetching mapped stored ranges,
   authenticating, decrypting, and comparing sliced plaintext to the source
   bytes (Section 6.3) — not by range arithmetic alone — including a range
   spanning a chunk boundary and a range in the final chunk.
6. Parity-over-ciphertext recovery is demonstrated: corrupt stored blocks of
   an encrypted object on the mock tape; the parity layer recovers keyless; keyed
   open then succeeds and fails closed before recovery.
7. A failed seal (digest mismatch, size mismatch, injected I/O error) is
   proven to produce no footer and no durable catalog reference; a failed
   build likewise (per [REMTAR1] §17).
8. Coverage-guided fuzzing of the envelope header parser, both CBOR
   decoders, the tar record loop, and whole-object open/verify reaches a
   corpus plateau with no panics, crashes, hangs, or unbounded allocations.
9. A live round-trip passes on the QuadStor VTL and on the MSL3040 for
   both representations at two distinct `chunk_size` values, including
   standard-`tar -b` extraction of the plaintext copy.
10. The 30-year drill passes for the plaintext representation
    ([REMTAR1] §17 criterion 6), and an independent exercise confirms an
    encrypted object can be opened from this document, the key material,
    and generic crypto libraries alone — no reference source.
11. Salt-derivation conformance is demonstrated: resealing the same
    canonical object under one epoch reproduces a byte-identical encrypted
    copy; sealing objects differing only in content, and objects differing
    only in `object_id`, yields distinct salts, keys, and ciphertexts; and
    a keyed open rejects an otherwise self-consistent object whose header
    salt is not the derived value (Sections 5.4.1, 5.9 step 4).

## 16. References

### 16.1. Normative

- [RFC2119] / [RFC8174] — BCP 14 requirement keywords.
- [RFC5869] — HMAC-based Extract-and-Expand Key Derivation Function (HKDF).
- [RFC8439] — ChaCha20 and Poly1305 for IETF Protocols.
- [RFC8949] — Concise Binary Object Representation (CBOR), STD 94.
- [POSIX-PAX] — IEEE Std 1003.1, `pax` Interchange Format.
- [REMTAR1] — *rem-tar-v1 Candidate Specification*
  (`docs/rem-tar-v1-candidate-specification.md`): the complete normative
  wire format of the plaintext stream, incorporated by reference as amended
  by Section 4.1.

### 16.2. Informative

- [AOF1] — *Archive Object Format Version 1, Candidate Specification*
  (`docs/aof1_candidate_specification.md`): the source of the encryption
  construction (Section 5); historical reference only — never published or
  deployed (Section 13).
- [AGE] — "The age format specification", C2SP: payload STREAM construction
  reference.
- [REMPARITY] — *rem-parity-1 Candidate Specification*
  (`docs/rem-parity-1-candidate-specification.md`): the tape parity layer
  ("Layer 3c" in internal Remanence documents).
- [PART-ORACLE] — Len, Grubbs, Ristenpart, "Partitioning Oracle Attacks",
  USENIX Security 2021.
- [AEAD-COMMIT] — Albertini et al., "How to Abuse and Fix Authenticated
  Encryption Without Key Commitment", USENIX Security 2022.
- `design-rem-archive-object-format.md` — the merge design rationale and
  work order (motivations; not re-litigated here).
- `amber-architecture.md` — reference-implementation architecture of the
  absorbed AEAD core.
- `docs/pfr-reference.md` — PFR mechanics and worked range examples.

---

## Appendix A. Resolved Conflicts Between the Source Specifications

Every point where the source documents disagreed, or were silent and a choice
was required, is recorded here so future revisions do not silently reverse
it. Where the design document (`design-rem-archive-object-format.md`) made
the call, it is cited; where it was silent, the rationale is given inline.

### A.1. AEAD chunk size: 64 KiB (AOF1) vs the 256 KiB body block

**Conflict.** AOF1 froze `chunk_size = 65536`. The design doc (§2.2, §9.3)
requires AEAD chunking aligned to the object's body block (default 256 KiB).

**Resolution.** The AEAD plaintext chunk size **is** the object's
`chunk_size` `C` (Section 5.6.1) — not a second constant. Rationale: (a) the
canonical object's length is always an exact multiple of `C` (the final zero
fill guarantees it), so plaintext chunk `i` coincides exactly with inner body
block `i` — the per-file `BodyLba` index addresses ciphertext with no second
offset grid and no short-final-chunk cases; (b) tape reads are
block-granular anyway, so 64 KiB sub-block chunks buy nothing on the primary
backend while quadrupling per-chunk tag/nonce bookkeeping; (c) tag overhead
is negligible either way (16/262144 ≈ 0.006%); (d) the STREAM counter bound
holds trivially for any legal `C` (Section 5.6.2). Trade-offs accepted and
documented: ciphertext chunks are `C + 16` bytes, so they do not land
tape-block-aligned — one inner block's ciphertext spans up to 3 stored
blocks (Section 6.4) — and on byte-addressed stores the minimum fetch for a
tiny range is `C + 16` rather than 64 KiB + 16, irrelevant for a media
workload whose restores are MB-scale. The alternative of `C − 16` plaintext
chunks (block-aligned ciphertext, misaligned plaintext) was rejected as it
destroys the 1:1 block↔chunk identity that makes PFR arithmetic and parity
reasoning simple.

### A.2. The encryption keyword: `REMANENCE.encryption` flag vs the envelope

**Conflict.** rem-tar-v1 reserved `REMANENCE.encryption` (with
`aes-gcm-256`-style values contemplated in early drafts, then re-scoped as a
pure refusal gate whose rationale was "payload-layer AOF1 owns
confidentiality", [REMTAR1] A.5). AOF1 carried encryption as a container
representation (`aead-stream-v1`).

**Resolution.** Encryption is the **envelope**, never an in-stream flag. The
keyword remains permanently `none` in every conformant stream (Section 4.1
item 2) and remains a Reader-MUST refusal gate. Rationale: flagging
encryption inside the global header would break byte-compatibility, break
the shared `plaintext_digest` (the two copies' canonical bytes would
differ), and put the marker *inside* the very bytes it claims are
encrypted. [REMTAR1] A.5's mechanism survives intact; its rationale is
updated — the RAO envelope, not AOF1-as-payload, now owns confidentiality.

### A.3. Digest definitions

**Conflict.** AOF1 defined `plaintext_digest` over an externally supplied
payload (a tar stream the orchestrator staged); rem-tar-v1 had no whole-
object digest pair, only `file_sha256`/`manifest_sha256` plus the external
catalog. The design doc requires the AOF1 split with the
shared-logical-identity property.

**Resolution.** `plaintext_digest` is SHA-256 of the **complete canonical
plaintext object** (Section 3.3) — the self-describing container stream
itself, not a bare inner tar. Consequences made explicit: for a plaintext
copy `stored_digest` = `plaintext_digest`; copies share the logical identity
iff they wrap identical canonical bytes (build once, fan out — Section 3.2);
rebuilt objects get new identities and `file_sha256` is the cross-rebuild
invariant. `stored_digest` stays external-only (A.10).

### A.4. Final-chunk and empty-payload semantics

**Conflict.** AOF1 permits a short final chunk and encodes an empty payload
as one empty final chunk.

**Resolution.** Both cases are unrepresentable in RAO: `plaintext_size` is a
positive exact multiple of `C`, every chunk is exactly `C` bytes, and
readers MUST reject metadata violating this (`InvalidMetadataField`). The
STREAM construction is unchanged — RAO objects simply occupy the subset of
its inputs where the final chunk is full. (A canonical object is never
empty: it contains at least the global header, manifest, and EOF.)

### A.5. HKDF labels

**Silent point.** The design doc says "reuse the construction verbatim"; it
does not say whether the literal `aof1-*` info labels carry over.

**Resolution.** New labels `rao1-object-v1` / `rao1-metadata-v1` /
`rao1-payload-v1` (Section 5.4). The construction — extract/expand
structure, header-hash binding, dual keys, zero metadata nonce, STREAM
nonces — is reused exactly; the labels are domain-separation parameters.
With AOF1 retired unpublished (A.13) the rename is simply correct branding,
plus free domain separation against any stray pre-release artifact sealed
under a shared root key. The absorbed AEAD core must therefore parameterize
labels and chunk size (Appendix D.3); the values are pinned by the
Section 14 vectors.

### A.6. Object framing: footer-terminated (AOF1) vs block-multiple (rem-tar)

**Conflict.** An AOF1 object ends at its footer, with trailing bytes
rejected and any block padding pushed to the embedding context; a rem-tar
object includes its final zero fill and is always a block multiple.

**Resolution.** The rem-tar rule generalizes to both representations: the
encrypted object's stored bytes include zero fill from the footer to the
next `C` multiple (Section 5.7), inside `stored_digest` and parity.
Rationale: byte-stable fanout — one byte string, identical on tape, disk,
and object store, with one `stored_digest`; uniform block geometry for parity;
no backend-specific framing. AOF1's `TrailingData` rule survives relocated:
bytes beyond `stored_size_bytes` are rejected. The footer retains its AOF1
role (completion detection), and the block-multiple framing makes the whole
geometry keyless-derivable, strengthening AOF1's weak keyless suffix check
into exact positional footer verification (Section 5.10). Tape filemark framing (one object = one
tape file, filemark by the parity layer) is unchanged from rem-tar; the tape block size
MUST equal `C` for encrypted copies too (Section 8.2) — new, flagged in
D.4.

### A.7. Metadata handling: AOF1 metadata frame vs the rem-tar manifest

**Conflict.** Each source had one metadata mechanism — AOF1 a small
integer-keyed encrypted CBOR frame, rem-tar a rich text-keyed cleartext CBOR
manifest — and they overlap (both can carry digests, identifiers,
application metadata).

**Resolution.** Both are kept, at different layers, with disjoint jobs: the
**manifest** (REM-CBOR, text keys, inside the canonical bytes) is the
object's per-file index and is consequently encrypted in the encrypted
representation — the confidentiality decision of the design doc §9.2; the
**envelope metadata frame** (RAO-CBOR, integer keys) carries only what
decryption itself needs (`plaintext_size`, `plaintext_digest`) plus
descriptive odds and ends. They are not merged: folding per-file metadata
into the envelope frame would recreate AOF1's weakness (index outside the
self-describing object) and bloat a frame whose smallness bounds hostile
input. AOF1's optional keys are dropped entirely — 16/17 because the header
`object_id` and the fixed payload type make them redundant, and 18/19/20
because variable envelope metadata breaks the zero-nonce argument under
derived salts (A.16); v1 metadata is exactly the four required keys.

### A.8. Naming and identifiers

**Decision point** (design doc §9.1: RAO vs `rem-tar-v2` vs other).

**Resolution (final, maintainer-confirmed).** The format is **Rem Archive
Object (RAO), version 1**: envelope magic `RAO1`, stream
`REMANENCE.format_id = rao-v1`, `schema_version 1.0`, file extension `.rao`.
Revision 1 of this document had retained the wire identifier `rem-tar-v1`
for byte compatibility with the predecessor candidate; the maintainer's
2026-06-11 direction — neither predecessor was ever published or deployed —
removed the only reason for that, and Revision 2 renames the identifier (the
sole wire difference from the rem-tar-v1 candidate layout; every other byte
is unchanged). Publicly the format is version 1; the internal candidates are
not part of its version history (Section 13, A.13). The `REMANENCE.*` pax
keyword namespace is retained: pax vendor keywords conventionally carry the
implementing project's name (compare `SCHILY.*`, `LIBARCHIVE.*`), and the
namespace is versioned by `format_id`, not by its prefix. The pair of
identifiers versions together (Section 10).

### A.9. Confidential manifest vs keyless self-description

**Decision point** (design doc §9.2; recommendation: confidential).

**Resolution.** Confidential. An encrypted RAO object leaks no structure
(Section 12.5); self-description holds *with the key*; keyless operation
keeps exactly the scrub/inventory surface of the plaintext header. The
alternative (cleartext manifest beside encrypted payloads) was rejected:
filenames, sizes, and counts are routinely sensitive for the offsite/cloud
copy, and a keyless catalog rebuild of encrypted media was judged worth less
than metadata confidentiality — the catalog and the plaintext working copy
already provide rebuild paths.

### A.10. `stored_digest` placement

**Tension.** The design doc §2.2 lists "`stored_digest` material" among the
plaintext header contents; AOF1 keeps `stored_digest` strictly external.

**Resolution.** Strictly external (catalog/master index), as in AOF1: a
digest over the complete stored bytes cannot live inside them, and a
truncated variant in-band would be a second, weaker integrity story. The
header field that *serves* keyless scrubbing is `object_id` — it lets a
scrubber look up the trusted external digest. Keyless scrub = external
`stored_digest` + (on tape) the parity layer's block CRCs.

### A.11. AAD contents

**Tension.** The work order's crypto checklist says "AAD binds object_id +
chunk index"; AOF1's construction uses empty AAD everywhere.

**Resolution.** Empty AAD, as in AOF1 (Section 12.3): both bindings already
exist structurally (header-hash key derivation binds the object including
`object_id`; the nonce binds index and finality). The checklist's *intent*
is satisfied; its *mechanism* is not duplicated.

### A.12. AOF1's `raw-v1` envelope

**Silent point.** Should RAO retain a raw (unencrypted) envelope around the
stream, as AOF1 had around its payload?

**Resolution.** No. The plaintext representation is the bare canonical
stream (Section 3.2): a raw envelope would add a header/footer that breaks
standard-`tar` extractability — the plaintext copy's reason to exist — in
exchange for framing the stream already provides (self-description,
digests). Uniform object framing across backends, AOF1's argument for
`raw-v1`, is supplied by the canonical stream itself.

### A.13. Predecessor compatibility dropped (maintainer decision, 2026-06-11)

**Conflict.** The design doc (§1.6) required preserving the ability to read
legacy AOF1 objects (and sequenced amber's retirement behind consumer
migration); Revision 1 of this document carried that as a normative MUST and
additionally froze the `rem-tar-v1` stream identifier for byte
compatibility.

**Resolution.** Overridden by maintainer direction: neither predecessor
format was ever published or used in production, so there is nothing to be
compatible *with*, and the public format is version 1. The AOF1 reader is
not ported; pre-release artifacts (harness-scenario objects, development
tapes, `.aof` files) are regenerated; the stream identifier is `rao-v1`
(A.8); predecessor inputs are refused by the existing gates (Section 13).
Consumer migration off the amber CLI remains real operational work, but it
is migration of *tools*, not of archived objects.

### A.14. Bootstrap manifest anchors are plaintext-copy-only (Revision 3)

**Gap (external review).** Revision 2 gave encrypted objects the same
bootstrap manifest anchors as plaintext ones (location, size, chunk count,
`manifest_sha256`) while Section 12.5 claimed encrypted copies reveal no
structure — contradictory: manifest size correlates directly with member
count, and the bootstrap is plaintext on the same tape the envelope
protects.

**Resolution.** Encrypted objects' bootstrap rows MUST NOT carry manifest
anchors; they carry envelope fields only (Section 8.2). The integrity anchor
for a decrypted manifest is the envelope's authenticated `plaintext_digest`,
which covers the manifest and every other canonical byte — strictly stronger
than an external digest — satisfying the [REMTAR1] §8.3 anchor obligation
(Section 7.1). What is forfeited is direct LOCATE-to-manifest on an
encrypted copy without a catalog: catalog-less recovery decrypts
sequentially, an acceptable cost on a recovery path over sequential media.
Off-tape catalogs are a separate trust domain (Section 12.5) and may store
what policy permits.

### A.15. Derived salts replace AOF1's random salts (Revision 6)

**Divergence.** AOF1 §7.3 required a fresh CSPRNG salt per object and made
salt freshness the construction's single uniqueness source — an absolute
operational requirement whose violation was catastrophic, and which forced a
fenced "unsafe" fixed-salt interface into the sealing API for test vectors.

**Resolution.** RAO derives the salt from the root key, the object
identifier, the content digest, and the metadata bytes (Section 5.4.1,
A.16). Rationale: (a) the catastrophic precondition — identical keys for
different plaintexts — now requires an identifier-reuse bug *combined with*
a SHA-256 digest collision or a 2^−128 truncated-PRF collision
(Section 12.1) rather than an RNG failure, and RNG failures are documented
history (Debian OpenSSL 2008, cloned-VM and fork-without-reseed entropy
reuse) while the hash and keyed-PRF assumptions are ones the format already
carries; (b) sealing
becomes deterministic, so resealing reproduces byte-identical encrypted
copies (byte-stable fanout extends to independently sealed copies) and test
vectors are reproducible by rule with no unsafe interface; (c) the wire
format and the AEAD construction are untouched — the stored bytes are
indistinguishable from a random-salt object, so "reuse the AOF1
construction verbatim" still holds where it matters. (As introduced in
Revision 6 the derivation was writer policy; Revision 7 made it
reader-verified on every keyed open — Section 5.9 step 4, A.16.) The
key-dependent Extract salt rests on the dual-PRF assumption for
HMAC-SHA-256 (Section 12.1). The residual disclosure — reseal equality — is
already public via the header `object_id`. Alternatives considered: a hedged
variant mixing CSPRNG bytes into the derivation (keeps reseals unlinkable;
rejected as solving a non-problem here while re-importing a pinned-input
test interface), and a misuse-resistant AEAD such as AES-GCM-SIV (rejected:
a new construction and new vectors for the same effective property).

### A.16. Envelope metadata is fixed in v1 and bound into the salt (Revision 7)

**Hazard (external review).** Revision 6 derived the salt from `object_id`
and `plaintext_digest` while Section 5.5.3 still defined optional metadata
keys (`producer`, `created_unix_seconds`, `application_metadata`).
Resealing the same object with different optional metadata of the same
encoded length would have reproduced the same salt, header hash, and
`metadata_key` — and the zero metadata nonce would then have encrypted two
*different* metadata plaintexts under one key, exactly the nonce reuse
RFC 8439 forbids. A timestamp key is the canonical trigger: a later reseal
changes the value but rarely the encoded length.

**Resolution, two layers.** (1) Version 1 removes the optional metadata
keys: a conformant writer emits exactly the four required keys, making the
metadata frame a deterministic function of the canonical object. The
discarded keys were redundant — the write timestamp lives in the canonical
global header, and application metadata belongs in the manifest's reserved
containers, inside the digest-covered canonical bytes. (2) Structurally,
SHA-256 of the metadata plaintext is bound into the salt derivation, and the
derivation is verified on every keyed open (Sections 5.4.1, 5.9 step 4) —
so a future 1.x metadata extension, or a defective sealer, cannot reach the
reused-nonce state in a readable object. Determinism (A.15) is preserved:
in version 1 the new derivation input is itself a function of the canonical
object.

## Appendix B. Changes from rem-tar-v1 and AOF1

### B.1. Changes from rem-tar-v1

The plaintext wire layout is unchanged except for one keyword value: the
global `REMANENCE.format_id` becomes `rao-v1` (Section 4.1, A.8).
Pre-release objects carrying the old identifier are regenerated, not read
(Section 13). The rest of the changes are scope and surroundings:

1. RAO becomes the format of record; the rem-tar-v1 candidate's layout is
   its plaintext representation (A.8).
2. An optional encrypted representation exists (Section 5); `rem-tar-v1`'s
   "no encryption, payloads are AOF1's problem" posture ([REMTAR1] §1.2
   "payload contents are opaque", A.5) is superseded by the envelope model.
   Payloads are now plain member files, not AOF1 objects.
3. The whole-object identity pair `plaintext_digest`/`stored_digest` is
   defined over the stream (Section 3.3); previously only per-file and
   manifest digests plus catalog conventions existed.
4. The address space vocabulary gains **inner** vs **stored** `BodyLba`
   (Section 2.3) — coincident for plaintext copies, related by Section 6.3
   arithmetic for encrypted ones.
5. The tape binding extends to encrypted copies (block size = `C`, filemark
   framing, parity over ciphertext) — Sections 8.2, 9.
6. New conformance roles: Sealer, Keyless Verifier; the Verifier profile
   extends to encrypted copies (Section 7.4).

### B.2. Changes from AOF1

The cryptographic construction is reused verbatim — HKDF-SHA-256 with
header-hash binding, dual metadata/payload keys, zero metadata nonce, the
age-style STREAM with 11-byte counter + final-flag nonces, fail-closed
discipline. Everything around it changes:

1. The payload is the self-describing RAO canonical stream, not an opaque
   tar; the per-file index AOF1 lacked is inside the encrypted payload
   (A.7).
2. New magic `RAO1`, 128-byte header (was 64): adds `format_version` and a
   64-byte `object_id` field; drops the `representation` byte (no raw
   envelope, A.12); `chunk_size` becomes a per-object variable equal to the
   body block (was frozen 65536, A.1).
3. New HKDF labels `rao1-*` (A.5).
4. `plaintext_size` constrained to positive multiples of `C`; no short or
   empty final chunks (A.4).
5. `plaintext_digest` redefined over the canonical container bytes (A.3).
6. Footer renamed `RAO1_STREAM_END.`; stored bytes extend past it with zero
   fill to a block multiple, covered by `stored_digest` (A.6).
7. Metadata schema: all optional keys dropped — v1 metadata is exactly the
   required keys 0–3 (A.7, A.16).
8. AOF1 itself is retired outright — never published, no reader carried
   forward (Section 13, A.13).
9. Salts are derived from (`root_key`, `object_id`, `plaintext_digest`,
   metadata bytes), not drawn from a CSPRNG, and the derivation is verified
   on keyed open; AOF1's fresh-salt rules, fenced fixed-salt test
   interface, and optional metadata keys are superseded (Sections 5.4.1,
   5.5.3; A.15, A.16).

## Appendix C. Worked Example (Informative)

This appendix derives the Section 14.1/14.2 expected values, exercising the
alignment equation, the manifest sizing, and the envelope geometry. It is
informative; the fixtures, once generated, are the conformance authority.

**RAO-TV-P1, `chunk_size` = 4096.** The global pax body's eight records
(Section 4.3 keywords with the TV-P1 values, `format_id` = `rao-v1`) measure
39 + 29 + 29 + 30 + 43 + 60 + 32 + 50 = 312 bytes, padding to 512; with its
`g` record the global header occupies bytes 0–1023.

*File 0* (`a/hello.txt`, 26 bytes): base pax records — `chunk_count` 27,
`compression` 30, `file_id` 58, `file_sha256` 90, `path` 20, `size` 11 —
total 236 bytes. Pax record offset `O` = 1024; the alignment equation
`O + 512 + R + 512 ≡ 0 (mod 4096)` gives `R ≡ 2048`; minimum-pad payload
236 + 18 = 254 rounds to 512 (wrong residue), so the target is `R` = 2048
and the pad record is `1812 REMANENCE.pad=` + 1792 spaces + LF (1812 = 4
digits + 1 space + 13 keyword + 1 `=` + 1792 + 1 LF; the fixed point holds).
Pax payload 236 + 1812 = 2048 exactly; the ustar header ends at 1024 + 512 +
2048 + 512 = 4096. Data at `BodyLba` 1; 26 bytes + record padding ends the
entry at 4608.

*File 1* (`b/pattern.bin`, 5000 bytes): base records 27 + 30 + 58 + 90 + 22
+ 13 = 240. `O` = 4608 → `R ≡ 2560 (mod 4096)` → `R` = 2560; pad record
`2320 REMANENCE.pad=` + 2300 spaces + LF; data at 8192 (`BodyLba` 2),
`chunk_count` 2; entry ends at 13312 (5000 bytes pad to 5120).

*Manifest*: REM-CBOR sizes — top-level overhead 1; `object_id` 48;
`chunk_size` 14; `file_entries` 13 + 1 + (194 + 197); `schema_version` 16;
`object_metadata` 17; `caller_object_id` 26; `external_references` 21 —
total **548 bytes** (file-entry maps: 194 and 197 bytes respectively, with
`size_bytes` 26 encoding as `0x18 0x1a` and 5000 as `0x19 0x13 0x88`).
Manifest pax base records (with `executable=false`, `is_manifest=true`,
`path` 33, `size` 12) total 310; `O` = 13312 → `R ≡ 2048` → pad record
`1738 REMANENCE.pad=` + 1718 spaces + LF; manifest data at 16384
(`BodyLba` 4), 548 bytes padding to 17408. Tar EOF (1024 bytes) ends at
18432; zero fill to **20480 = 5 blocks**.

**RAO-TV-E1.** `P` = 20480, `C` = 4096 → `chunk_count` 5. Metadata
plaintext: `a4 00 01 01 19 50 00 02 66 ... 03 58 20 ...` = 1 + 2 + 4 + 8 +
35 = **50 bytes** → `M` = 66. Payload frame = 20480 + 80 = 20560 bytes at
offset 194; `footer_offset` = 20754; stored pre-fill length 20770; fill
3806 → **`stored_size_bytes` 24576 = 6 blocks**. The metadata zero nonce
does not collide with any payload nonce here (chunk 0 is non-final, nonce
`00…00 00` — the collision the key separation guards, Section 12.2).

## Appendix D. Open Questions and Recommendations

Points needing the maintainer's confirmation, each with the specification
author's recommendation. Items 1–2 gate freeze. Resolved since Revision 1:
the format name and identifiers (A.8) and predecessor compatibility (A.13).

1. **Pinned-at-generation vector values** (Section 14). The cryptographic
   outputs (digests, derived keys, header hashes, exact frame bytes) must be
   generated, cross-checked against the derivable values of Appendix C, and
   frozen into `fixtures/rao/` and this document; until then the spec is
   Candidate by definition. *Recommendation:* generate with the reference
   implementation, then **independently re-derive the envelope values**
   (header hash, HKDF keys, chunk decryption, `stored_digest`) with a
   minimal second implementation in another language/library before
   freezing — dual-implementation pinning, so a reference-implementation
   bug cannot be frozen into the conformance anchor. The plaintext side
   already gets this independence from the GNU tar / bsdtar / `tarfile`
   interop gates.
2. **`object_id` ≤ 64 bytes in the envelope header** (Section 5.2). The
   plaintext stream allows unbounded `object_id`; the envelope caps it — a
   frozen-field choice that must be settled before freeze.
   *Recommendation: keep the 64-byte UTF-8 field.* Identifiers are 36-byte
   UUID strings today, leaving ample headroom; a human-readable identifier
   in the header is genuinely useful for keyless disaster inventory (it is
   legible in a hex dump of stored block 0); and the compact alternative —
   a 16-byte binary UUID — would couple the envelope to one identifier
   syntax for a 48-byte saving that is irrelevant against a 256 KiB block.
   Confirm no orchestrator will ever mint longer identifiers, then freeze.
3. **Absorbed AEAD core parameterization** (A.5). *Recommendation:* absorb
   `amber_core` as a `remanence-aead` crate exposing the HKDF info labels
   and the STREAM chunk size as construction parameters, instantiated once
   with the RAO values (`rao1-*`, `C`). With AOF1 retired (A.13), no
   dual-parameter legacy mode is needed; verify during the port that
   nothing else in the core assumed 64 KiB (buffer sizing, counter width
   assumptions, test fixtures).
4. **Tape block size = `chunk_size` for encrypted copies** (Section 8.2).
   *Recommendation: keep the MUST* — uniform geometry with plaintext
   copies, assumed by Section 6.4. Operational corollary to confirm against
   the parity-layer and `tape-block-size-config` designs: a copy must be
   written to a pool whose configured tape block size equals the object's
   `chunk_size`; in practice one fleet-wide `chunk_size` (the 256 KiB
   default) makes this a non-issue.
5. **`caller_object_id` keyless visibility** (Section 12.5). Keyless
   inventory of encrypted media recovers `object_id` only.
   *Recommendation: keep `caller_object_id` confidential* (inside the
   encrypted stream). Caller identifiers routinely embed project and asset
   names — precisely the metadata the envelope exists to hide; keyless
   inventory by `object_id` plus the catalog covers the operational need.
6. **Parity-layer bootstrap row extension for encrypted objects**
   (Section 9 item 4):
   representation marker, `key_id`, `metadata_frame_len`, stored block
   count. *Recommendation: adopt* — a small additive revision of
   [REMPARITY]'s bootstrap schema, scheduled with the implementation work
   order rather than blocking this document.
7. **Pre-publication editorial merge.** This document incorporates
   [REMTAR1] by reference under its internal title. *Recommendation:*
   before any public release, retitle/absorb the stream specification as
   "RAO version 1, Part II: the plaintext stream" (or inline it as an
   annex) so the published artifact is one self-contained document with no
   internal codenames; purely editorial, no wire change, deferred until
   after freeze so the two documents stay stable during implementation.
