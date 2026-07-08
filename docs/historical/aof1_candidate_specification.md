# Archive Object Format Version 1 (AOF1)

## Candidate Specification

| | |
| --- | --- |
| Status | Candidate |
| Revision | 2 |
| Date | 2026-06-10 |
| Supersedes | Revision 1 (2026-06-06) |
| Format magic | `AOF1` |
| Default file extension | `.aof` |
| Reference implementation | Amber (`amber_core`, `amber_cli`) |

## Abstract

This document specifies Archive Object Format version 1 (AOF1), a
backend-independent binary container for large archival payloads. An AOF1
object wraps a payload byte stream in a fixed plaintext header, a
deterministic metadata frame, a payload frame, and a plaintext completion
footer. AOF1 defines two representations: `raw-v1`, which stores the payload
in plaintext, and `aead-stream-v1`, which encrypts and authenticates both
metadata and payload using HKDF-SHA-256 key derivation and a chunked
ChaCha20-Poly1305 stream construction. The format is designed for single-pass
creation, byte-stable fanout to multiple storage backends, efficient
linear-tape storage, partial file restore, and long-term recovery from this
specification and its static test vectors alone.

## Table of Contents

1. [Introduction](#1-introduction)
2. [Conventions and Terminology](#2-conventions-and-terminology)
3. [Object Layout](#3-object-layout)
4. [Fixed Public Header](#4-fixed-public-header)
5. [Metadata](#5-metadata)
6. [Key Identification and the Key Registry](#6-key-identification-and-the-key-registry)
7. [AEAD Key Derivation](#7-aead-key-derivation)
8. [Payload Frame](#8-payload-frame)
9. [Completion Footer](#9-completion-footer)
10. [Stored Digest](#10-stored-digest)
11. [Sealing](#11-sealing)
12. [Opening and Verification](#12-opening-and-verification)
13. [Partial File Restore](#13-partial-file-restore)
14. [Errors](#14-errors)
15. [Implementation Requirements](#15-implementation-requirements)
16. [Storage Backend Contract](#16-storage-backend-contract)
17. [Security Considerations](#17-security-considerations)
18. [Operational Considerations (Informative)](#18-operational-considerations-informative)
19. [IANA Considerations](#19-iana-considerations)
20. [Test Vector Requirements](#20-test-vector-requirements)
21. [Candidate Freeze Criteria](#21-candidate-freeze-criteria)
22. [References](#22-references)

Appendix A. [Changes from Revision 1 (Informative)](#appendix-a-changes-from-revision-1-informative)

---

## 1. Introduction

### 1.1. Purpose and Design Goals

AOF1 defines a container for large archival payloads that is independent of
any specific storage backend. Storage systems treat AOF1 objects as opaque
bytes. An AOF1 object can represent either:

- `raw-v1`: plaintext payload bytes with deterministic metadata and a
  completion footer; or
- `aead-stream-v1`: encrypted payload bytes with encrypted deterministic
  metadata and an authenticated chunked payload stream.

The primary design goals are:

1. **Single-pass creation.** A writer that already knows the payload size and
   digest can produce a complete object in one pass with no seeking or
   backfilling.
2. **Stable byte representation.** A sealed object is a fixed byte string
   that can be copied verbatim to multiple backends and compared
   byte-for-byte.
3. **Efficient linear-tape storage and partial file restore (PFR).** Payload
   byte ranges map to stored byte ranges by closed-form arithmetic.
4. **Explicit separation of identities.** The logical plaintext identity
   (`plaintext_digest`) is distinct from the stored byte identity
   (`stored_digest`).
5. **Long-term recoverability.** A future implementer holding only this
   document and the static test vectors can read every conformant object.

If both raw and encrypted copies of the same logical payload exist, they are
separate AOF1 objects that share a `plaintext_digest`; an external master
index joins them as copies of the same logical asset.

### 1.2. Relationship to Amber and Adjacent Systems

AOF1 is the format; Amber is the reference implementation. AOF1 does not
depend on Amber, and any conformant tool may read or write AOF1 objects.

AOF1 is deliberately independent of orchestration systems (such as Sutradhara),
storage backends (such as Remanence, LTO libraries, object stores, or disk
pools), and key-management infrastructure. The division of responsibility is:

- AOF1 implementations own object framing and optional object encryption.
- Orchestrators own payload creation, metadata preparation, fanout, and
  scheduling.
- Storage backends store AOF1 objects as opaque bytes and verify them by
  `stored_digest` (Section 10).
- The key registry (Section 6) owns root key material and its lifecycle.

### 1.3. Non-Goals

AOF1 itself performs no compression, stores no wrapped per-object data
encryption keys, defines no catalog or index format, and specifies no network
protocol. See Section 13.5 for the interaction between compression and
partial file restore.

## 2. Conventions and Terminology

### 2.1. Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174]
when, and only when, they appear in all capitals, as shown here.

### 2.2. Conformance Targets

This document places requirements on the following roles. A single
implementation may fill several roles.

- **Writer**: produces (seals) AOF1 objects (Section 11).
- **Reader**: opens AOF1 objects and recovers plaintext (Section 12).
- **Verifier**: validates AOF1 objects with access to key material
  (Section 12).
- **Keyless Verifier**: validates the public structure of AOF1 objects and
  computes `stored_digest` without key material (Section 12.3).
- **Restorer**: maps plaintext byte ranges to stored byte ranges for partial
  file restore (Section 13).

### 2.3. Definitions

- **Payload**: the byte stream wrapped by the AOF1 container. In the intended
  media-archive workflow this is normally an uncompressed tar stream.
- **Plaintext payload**: the payload bytes before AOF1 encryption. In
  `raw-v1`, the stored payload bytes are the plaintext payload bytes.
- **Metadata frame**: the bytes immediately after the fixed public header. In
  `raw-v1`, this is deterministic CBOR. In `aead-stream-v1`, this is a
  one-shot AEAD ciphertext over deterministic CBOR.
- **Stored bytes**: the exact bytes of a complete AOF1 object, from byte 0 of
  the header through the final footer byte.
- **`plaintext_digest`**: SHA-256 over the plaintext payload bytes only.
- **`stored_digest`**: SHA-256 over the complete AOF1 stored bytes.
- **Key registry**: the external system that maps a 16-byte AOF1 key
  identifier to root key material or to an HSM/KMIP/escrow retrieval
  procedure.
- **AOF1-CBOR**: the deterministic CBOR subset defined in Section 5.1.

### 2.4. Integer and Byte Conventions

All fixed-width integers in the public header are unsigned and encoded in
big-endian (network) byte order. All byte offsets in this document are
zero-based. `MiB` denotes 2^20 bytes. Hexadecimal values are prefixed with
`0x`.

All derived quantities in this document (offsets, frame lengths, chunk
counts) are defined over unsigned 64-bit arithmetic. Overflow handling is
specified in Section 8.3.

### 2.5. Constants

| Constant | Value | Meaning |
| --- | --- | --- |
| `AOF1_HEADER_LEN` | 64 | Fixed public header length in bytes |
| `AOF1_CHUNK_SIZE` | 65536 | AEAD plaintext chunk size in bytes |
| `AOF1_TAG_LEN` | 16 | Poly1305 tag length in bytes |
| `AOF1_NONCE_LEN` | 12 | ChaCha20-Poly1305 nonce length in bytes |
| `AOF1_KEY_LEN` | 32 | Derived AEAD key length in bytes |
| `AOF1_MAX_METADATA_FRAME_LEN` | 16777216 (16 MiB) | Maximum stored metadata frame length |
| `AOF1_MAX_CBOR_NESTING_DEPTH` | 32 | Maximum metadata nesting depth (Section 5.2) |
| `AOF1_MAX_METADATA_ITEMS` | 65536 | Maximum metadata data-item count (Section 5.2) |
| `AOF1_FOOTER` | `AOF1_STREAM_END.` | 16-byte completion footer (Section 9) |

## 3. Object Layout

### 3.1. Frame Sequence

An AOF1 object is a binary byte string with the following layout:

```text
+-------------------------------+
| Fixed public header, 64 bytes |
+-------------------------------+
| Metadata frame, N bytes       |
+-------------------------------+
| Payload frame, variable       |
+-------------------------------+
| Completion footer, 16 bytes   |
+-------------------------------+
```

The header is always plaintext. The footer is always plaintext. The metadata
and payload representations depend on the representation mode.

### 3.2. Representations

| Wire value | Name | Metadata frame | Payload frame |
| --- | --- | --- | --- |
| `0x00` | `raw-v1` | Deterministic CBOR plaintext | Plaintext payload bytes |
| `0x01` | `aead-stream-v1` | One-shot AEAD ciphertext over deterministic CBOR | Chunked AEAD stream |

### 3.3. Object Boundary and Trailing Data

An AOF1 object comprises exactly the four frames of Section 3.1 and ends at
the final footer byte. The format reserves no meaning for bytes following
the footer.

When the input presented to a Reader or Verifier is declared to contain
exactly one AOF1 object — for example, a regular file with the `.aof`
extension, or a stream passed to a whole-object `open` or `verify`
operation — the implementation MUST confirm that the input ends immediately
after the completion footer and MUST reject any additional bytes with
`TrailingData` (Section 14). A Keyless Verifier cannot locate the footer
offset (Section 12.3) and is exempt from this check; it consumes the input
to EOF, and trailing bytes surface as a `stored_digest` mismatch against the
trusted catalog value.

A context that embeds AOF1 objects inside a larger stream (for example, a
tape extent with trailing padding, or a concatenated container) MUST delimit
each object externally and present exactly the object's stored bytes to the
AOF1 implementation. AOF1 itself provides no in-band mechanism for locating
object boundaries other than parsing from a known start offset.

This rule exists so that `stored_digest` (Section 10) is well defined over
any input accepted by a whole-object operation.

## 4. Fixed Public Header

AOF1 has a fixed 64-byte public header. Readers MUST read exactly 64 bytes
for the header and MUST reject any input whose `header_len` field is not
exactly 64.

Header changes that alter length, parsing, compression, payload
interpretation, metadata semantics, or cryptographic behavior require a new
format magic such as `AOF2`. AOF1 does not support dynamic header extension.

### 4.1. Layout

| Offset | Length | Name | Type | Description |
| --- | ---: | --- | --- | --- |
| `0x00` | 4 | `magic` | ASCII | MUST be `AOF1`. |
| `0x04` | 2 | `header_len` | `uint16` | MUST be `64`. |
| `0x06` | 1 | `representation` | `uint8` | `0x00` = `raw-v1`; `0x01` = `aead-stream-v1`. |
| `0x07` | 1 | `suite_id` | `uint8` | `0x00` = none; `0x01` = HKDF-SHA-256 + ChaCha20-Poly1305. |
| `0x08` | 4 | `chunk_size` | `uint32` | `0` for raw; `65536` for AEAD. |
| `0x0C` | 4 | `flags` | `uint32` | MUST be zero in AOF1. |
| `0x10` | 16 | `key_id` | bytes | Opaque archive key identifier. |
| `0x20` | 16 | `hkdf_salt` | bytes | Per-object salt for AEAD mode. |
| `0x30` | 8 | `metadata_frame_len` | `uint64` | Number of stored metadata frame bytes. |
| `0x38` | 8 | `reserved` | bytes | MUST be all zero in AOF1. |

### 4.2. Frozen Fields

The `header_len`, `chunk_size`, and `suite_id` fields are frozen for the
lifetime of the `AOF1` magic: each has exactly one valid value per
representation (Section 4.3). They carry no negotiable information.

These fields exist for self-description and damage detection, not for
algorithm or parameter agility. A reader recovering a damaged archive can
confirm structural assumptions from the header alone, and a corrupted byte in
any frozen field is detected as a hard parse error rather than silently
changing interpretation. Future revisions of this document MUST NOT assign
additional valid values to these fields; any such change requires a new
format magic (Section 17.8).

### 4.3. Mode and Suite Invariants

The following combinations are valid. Readers MUST reject any header that
does not match one of these combinations exactly, using the error mapping of
Section 14.2.

#### 4.3.1. `raw-v1`

```text
representation      = 0x00
suite_id            = 0x00
chunk_size          = 0
key_id              = 16 zero bytes
hkdf_salt           = 16 zero bytes
flags               = 0
reserved            = 8 zero bytes
metadata_frame_len  = 1..=AOF1_MAX_METADATA_FRAME_LEN
```

`raw-v1` provides no cryptographic confidentiality and no tamper resistance
by itself (Section 17.1). It is useful for key-free local archive copies and
for uniform object framing.

The lower bound of 1 on `metadata_frame_len` is structural: a metadata frame
exists in every object, and the smallest CBOR item is one byte. The required
metadata schema (Section 5.3) produces a larger frame in every valid object;
schema validation occurs after CBOR validation and is not a header-level
concern.

#### 4.3.2. `aead-stream-v1`

```text
representation      = 0x01
suite_id            = 0x01
chunk_size          = 65536
key_id              = 16-byte nonzero opaque key identifier
hkdf_salt           = 16-byte nonzero random salt
flags               = 0
reserved            = 8 zero bytes
metadata_frame_len  = 17..=AOF1_MAX_METADATA_FRAME_LEN
```

The all-zero `key_id` and the all-zero `hkdf_salt` are reserved for raw mode.
Readers MUST reject an `aead-stream-v1` header containing an all-zero
`key_id` with `InvalidKeyIdentifier`, and an all-zero `hkdf_salt` with
`InvalidSalt`.

The lower bound of 17 on `metadata_frame_len` is structural: at least one
metadata ciphertext byte plus the 16-byte Poly1305 tag. As in raw mode, the
required metadata schema produces a larger frame in every valid object.

Salt generation requirements are specified in Section 7.3.

### 4.4. Metadata Frame Length Bounds

AOF1 metadata is intentionally small object metadata, not a storage-backend
catalog and not a full PFR table. Implementations MUST support
`metadata_frame_len` values up to `AOF1_MAX_METADATA_FRAME_LEN` (16 MiB).
AOF1 objects MUST NOT contain metadata frames larger than 16 MiB, and readers
MUST reject larger declared values with `MetadataFrameLengthInvalid`.

Large archival indexes and PFR maps SHOULD live in an external index, not
inside each AOF1 object.

### 4.5. Header Validation

A reader MUST validate every requirement of Sections 4.1 through 4.4 before
processing the metadata frame. The RECOMMENDED validation order is:

1. Read exactly 64 bytes (`UnexpectedEof` on short input).
2. `magic` (`InvalidMagicBytes`).
3. `header_len` (`InvalidHeaderLength`).
4. `representation` value known (`InvalidRepresentation`).
5. `suite_id` value known (`InvalidModeSuiteCombination`).
6. `flags` zero, `reserved` zero (`ReservedBytesNotZero`).
7. `metadata_frame_len` within the global maximum
   (`MetadataFrameLengthInvalid`).
8. Mode/suite invariants of Section 4.3, including `key_id`, `hkdf_salt`,
   and per-mode `metadata_frame_len` bounds.

When an input violates multiple requirements simultaneously, a reader MAY
report any one error whose condition holds; this document does not mandate a
detection order for multi-fault inputs. Each conformance test vector
(Section 20) contains exactly one fault so that the expected error is
unambiguous.

## 5. Metadata

### 5.1. The AOF1 Deterministic CBOR Subset (AOF1-CBOR)

The metadata plaintext is a single CBOR [RFC8949] data item restricted to the
deterministic subset defined in this section, called AOF1-CBOR. AOF1-CBOR is
the RFC 8949 Core Deterministic Encoding Requirements applied to a restricted
item repertoire.

**Item repertoire.** An AOF1-CBOR item MUST be one of:

| Major type | Items permitted |
| --- | --- |
| 0 | Unsigned integers `0` through `2^64 - 1` |
| 1 | Negative integers `-1` through `-2^64` |
| 2 | Definite-length byte strings |
| 3 | Definite-length UTF-8 text strings |
| 4 | Definite-length arrays of AOF1-CBOR items |
| 5 | Definite-length maps of AOF1-CBOR key/value pairs |
| 7 | Simple values `false` (20), `true` (21), and `null` (22) only |

The following MUST NOT appear anywhere in AOF1-CBOR, and decoders MUST reject
them with `InvalidCborEncoding`:

- Tags (major type 6), regardless of tag number.
- Indefinite-length items of any major type (additional information 31).
- Floating-point values of any width (major type 7, additional
  information 25, 26, or 27).
- The simple value `undefined` (23), one-byte-encoded simple values
  (major type 7, additional information 24), and all simple values other
  than `false`, `true`, and `null`.

Note that this repertoire is deliberately narrower than what RFC 8949
deterministic encoding permits (which allows tags, for example). The
restriction makes independent implementations trivially interoperable and
keeps the long-term recovery surface small. An item that is valid
deterministic CBOR but outside this repertoire is not valid AOF1-CBOR.

**Encoding requirements.** All of the following MUST hold, and decoders MUST
reject violations with `InvalidCborEncoding`:

1. Every integer value and every length argument MUST use the shortest
   possible encoding (preferred serialization): additional information 24
   only for values ≥ 24, 25 only for values ≥ 256, 26 only for values
   ≥ 65536, and 27 only for values ≥ 2^32.
2. Text strings MUST be valid UTF-8.
3. Map keys MUST be sorted in strictly ascending order by the bytewise
   lexicographic comparison of their deterministic encodings.
4. Duplicate map keys MUST NOT appear. (Strictly ascending order implies
   this; it is stated separately so that decoders reject equality
   explicitly rather than relying on sort-order checks alone.)
5. Map keys MAY be any AOF1-CBOR item, except at the top level of the
   metadata item, where Section 5.3 requires unsigned integer keys.
6. The metadata item MUST occupy the entire metadata plaintext exactly:
   decoders MUST reject trailing bytes after the item.

**Validation strategy.** An implementation MUST NOT assume that a
general-purpose CBOR library emits or rejects AOF1-CBOR by default.
Canonical-form validation MUST be performed over the original encoded bytes.
A validation strategy that decodes into a typed model and re-encodes for
comparison MUST NOT drop unknown top-level keys (Section 5.3) in the
process, because doing so would cause valid objects to fail validation or
unknown metadata to be silently discarded.

### 5.2. Structural Limits

The following limits are format-level rules. They exist so that the memory
required to decode a metadata frame is bounded by a small multiple of the
frame size, independent of how the frame's bytes are arranged; without them,
a dense encoding can force a decoded representation roughly 30 times larger
than the frame itself. Because the limits are part of the format, all
conformant readers accept and reject exactly the same objects; an
implementation-defined limit would create interoperability gaps.

1. **Nesting depth.** The top-level metadata item has depth 1. An item
   contained in an array or map at depth *d* has depth *d* + 1. The depth of
   any item MUST NOT exceed `AOF1_MAX_CBOR_NESTING_DEPTH` (32).
2. **Item count.** The total number of data items in the metadata item —
   counting the top-level item and, recursively, every array element, every
   map key, and every map value as one item each — MUST NOT exceed
   `AOF1_MAX_METADATA_ITEMS` (65536).

Writers MUST NOT produce metadata exceeding these limits. Readers MUST
reject violations with `InvalidCborEncoding`. Readers SHOULD enforce both
limits incrementally during decoding rather than after constructing a
decoded representation.

### 5.3. Metadata Schema

The top-level metadata item MUST be an AOF1-CBOR map whose keys are all
unsigned integers. A reader MUST reject a non-map top-level item or a
top-level key of any other type with `InvalidCborEncoding`.

#### 5.3.1. Required Keys

| Key | Name | Type | Constraint |
| ---: | --- | --- | --- |
| `0` | `metadata_version` | unsigned integer | MUST be `1`. |
| `1` | `plaintext_size` | unsigned integer | Number of plaintext payload bytes. |
| `2` | `plaintext_digest_alg` | text string | MUST be `sha256`. |
| `3` | `plaintext_digest` | byte string | MUST be exactly 32 bytes: the SHA-256 digest of the plaintext payload. |

A reader MUST reject metadata missing any required key with
`MissingRequiredMetadataField`. A reader MUST reject a required key whose
value has the wrong type or violates its constraint — including
`metadata_version` ≠ 1 and `plaintext_digest_alg` ≠ `"sha256"` — with
`InvalidMetadataField`.

The `plaintext_digest` MUST be computed over the plaintext payload bytes
only. It MUST NOT include the AOF1 header, metadata frame, payload AEAD
tags, or footer.

#### 5.3.2. Optional Keys

| Key | Name | Type | Description |
| ---: | --- | --- | --- |
| `16` | `object_id` | byte string | Opaque object or rendering identifier assigned by the orchestrator. |
| `17` | `payload_media_type` | text string | Example: `application/x-tar`. |
| `18` | `producer` | text string | Tool name and version. |
| `19` | `created_unix_seconds` | unsigned integer | POSIX timestamp, seconds only. (Times before 1970 are not representable; this format targets newly created archive objects.) |
| `20` | `application_metadata` | map | Application-specific descriptive metadata. |

If an optional key is present with the wrong type — including key `20`
present and not a map — a reader MUST reject it with
`InvalidMetadataField`.

#### 5.3.3. Unknown Keys

AOF1 defines no optional semantic keys that alter payload interpretation.
Top-level unsigned integer keys outside the defined set are descriptive
application metadata:

- Readers MUST NOT reject metadata solely because it contains unknown
  top-level unsigned integer keys.
- Unknown keys MUST NOT alter payload parsing, compression, decryption,
  chunking, or interpretation in any conformant implementation.
- Implementations that re-emit metadata SHOULD preserve unknown keys and
  their values bytewise.
- Writers MUST NOT emit a defined key (Sections 5.3.1–5.3.2) with a type or
  value other than as specified.

Any future change that gives a metadata key payload-interpretation semantics
requires a new format magic (`AOF2`).

### 5.4. Raw Metadata Frame

In `raw-v1`, the metadata frame is the AOF1-CBOR metadata plaintext, stored
verbatim. `metadata_frame_len` equals the plaintext length:

```text
metadata_frame_len = len(aof1_cbor_metadata)
```

### 5.5. Encrypted Metadata Frame

In `aead-stream-v1`, the metadata frame is a single ChaCha20-Poly1305
[RFC8439] AEAD ciphertext over the AOF1-CBOR metadata plaintext:

```text
metadata_frame = metadata_ciphertext || metadata_tag
```

where `metadata_tag` is the 16-byte Poly1305 tag. ChaCha20-Poly1305 is
length-preserving apart from the tag, so:

```text
metadata_frame_len = len(aof1_cbor_metadata) + 16
```

The metadata AEAD parameters are:

```text
key   = metadata_key        (Section 7.1)
nonce = 12 zero bytes
aad   = empty byte string
```

The zero nonce is safe if and only if `metadata_key` is unique per object.
That uniqueness is provided entirely by the per-object `hkdf_salt`
(Section 7.3): the salt is the only per-object entropy in the key
derivation, because the public header deliberately contains no
payload-dependent data (Section 17.5). See Section 17.3 for the
consequences of salt reuse.

Note also that this zero nonce is byte-identical to the nonce of payload
chunk 0 when that chunk is non-final (Section 8.2.2). This is safe only
because `metadata_key` and `payload_key` are distinct (Section 7.4); the key
separation is a load-bearing security requirement, not a stylistic choice.

The AAD is empty because the header is already cryptographically bound to the
ciphertext through key derivation (Section 7.1); binding it again as AAD
would be redundant.

## 6. Key Identification and the Key Registry

The `key_id` field is a 16-byte opaque archive key identifier. AOF1 assigns
no internal semantics to it.

The key registry is an external system responsible for:

- generating `key_id` values;
- mapping each `key_id` to root key material or to a key retrieval process;
- maintaining key epoch lifecycle records; and
- preserving HSM/KMIP/escrow recovery instructions outside the AOF1 object.

Human-readable labels such as `Epoch_2026_A` MUST NOT be stored in the fixed
header. Such labels MAY exist in the external key registry.

The trust placed in the key registry is part of the AOF1 security model; see
Sections 17.2 and 17.6.

## 7. AEAD Key Derivation

### 7.1. Construction

AEAD mode uses HKDF with SHA-256 as specified by [RFC5869]. In this
document:

```text
HKDF(ikm, salt, info, len)
```

means HKDF-Extract using SHA-256 with `salt` and `ikm`, followed by
HKDF-Expand using `info` and output length `len`. An empty `salt` is
equivalent, per RFC 5869, to a salt of 32 zero bytes.

Let:

```text
header_bytes = the exact 64 bytes stored at offsets 0x00..0x3F
header_hash  = SHA-256(header_bytes)
root_key     = key material retrieved using key_id (Section 7.2)
salt         = hkdf_salt from the header (Section 7.3)
```

The derivation labels are ASCII byte strings with no terminator:
`aof1-object-v1` (14 bytes), `aof1-metadata-v1` (16 bytes), and
`aof1-payload-v1` (15 bytes).

The AEAD keys are derived as:

```text
object_secret = HKDF(
    ikm  = root_key,
    salt = salt,
    info = "aof1-object-v1" || header_hash,
    len  = 32
)

metadata_key = HKDF(
    ikm  = object_secret,
    salt = empty,
    info = "aof1-metadata-v1",
    len  = 32
)

payload_key = HKDF(
    ikm  = object_secret,
    salt = empty,
    info = "aof1-payload-v1",
    len  = 32
)
```

Because `header_hash` is bound into the derivation, changing any header
field — including a structurally valid change such as flipping a salt bit or
substituting another epoch's `key_id` — changes the derived keys, and
metadata or payload authentication MUST then fail. Conformance test vectors
exercise this property (Section 20.2).

### 7.2. Root Key Requirements

Implementations MUST reject root key material shorter than 32 bytes with
`InvalidRootKey`. An implementation MUST NOT silently derive from a shorter
key.

A root key SHOULD be exactly 32 uniformly random bytes. The 32-byte length
floor is the enforceable proxy for the real requirement, which is that the
root key contain at least 256 bits of entropy; no implementation can measure
entropy, so generation policy is the key registry's responsibility. AOF1
does not define how root keys are generated, stored, escrowed, or retrieved.

Implementations SHOULD hold root and derived key material in memory that is
zeroized when no longer needed, and MUST NOT write key material to logs or
diagnostics.

### 7.3. Salt Requirements

For every sealed `aead-stream-v1` object, the Writer:

1. MUST generate a fresh `hkdf_salt` from a cryptographically secure random
   number generator;
2. MUST NOT reuse an `hkdf_salt` value across objects under the same root
   key, whether deliberately, through caching, or through deterministic
   derivation; and
3. MUST discard an all-zero generated salt and generate another (the
   all-zero value is reserved for raw mode, Section 4.3.2).

These rules are the foundation of the entire AEAD design: per-object key
uniqueness — and therefore the safety of the zero metadata nonce
(Section 5.5) and of the counter-based payload nonces (Section 8.2.2) —
derives from salt freshness and from nothing else. Section 17.3 describes
the total loss of confidentiality and authenticity that follows from salt
reuse.

An API that allows a caller to supply a fixed salt (for example, to
regenerate deterministic test vectors) MUST be prominently documented as
unsafe for production use and SHOULD be separated from the normal sealing
interface in a way that prevents accidental selection.

### 7.4. Key Separation

`metadata_key` and `payload_key` MUST be derived with the distinct `info`
labels of Section 7.1 and MUST NOT be unified, derived from one another, or
replaced by a single key. The metadata zero nonce (Section 5.5) collides
with the payload chunk-0 non-final nonce (Section 8.2.2) by construction;
only the key separation prevents that collision from being a nonce-reuse
vulnerability. Future revisions MUST NOT "simplify" the key schedule by
merging these keys.

## 8. Payload Frame

The payload frame begins immediately after the metadata frame:

```text
payload_frame_offset = 64 + metadata_frame_len
```

The payload frame length is not stored; it is derived from `plaintext_size`
in the authenticated (or, in raw mode, validated) metadata. Readers MUST
process exactly the derived payload frame and MUST NOT scan for the footer.

### 8.1. Raw Payload Frame

In `raw-v1`, the payload frame is exactly `plaintext_size` plaintext payload
bytes. The footer begins at:

```text
footer_offset = 64 + metadata_frame_len + plaintext_size
```

### 8.2. AEAD Payload Frame

In `aead-stream-v1`, the payload frame uses the age-style STREAM construction
[AGE] with ChaCha20-Poly1305 and 64 KiB plaintext chunks. AOF1 uses the
chunking rules of the age payload construction, but an AOF1 object is not an
age file and is not expected to be decryptable by age implementations.

Parameters:

```text
chunk_size = 65536
key        = payload_key      (Section 7.1)
aad        = empty byte string
```

#### 8.2.1. Chunking Rules

- A non-final plaintext chunk MUST be exactly 65536 bytes.
- The final plaintext chunk MAY be shorter than 65536 bytes.
- The final plaintext chunk MUST NOT be empty unless the entire payload is
  empty.
- An empty payload is encoded as exactly one empty final chunk
  (`final_flag = 0x01`); its encrypted form is the 16-byte tag alone.
- A positive payload whose size is an exact multiple of 65536 is encoded
  with its final full chunk marked final. No additional empty final chunk
  is appended.
- Decryption MUST fail if EOF is reached before an authenticated final chunk
  has been processed (`MissingFinalChunk`).

#### 8.2.2. Chunk Structure and Nonce Construction

Each encrypted chunk is:

```text
chunk_ciphertext || chunk_tag
```

where `chunk_tag` is the 16-byte Poly1305 tag. For each chunk:

```text
nonce = 11-byte big-endian chunk counter || final_flag
```

- The chunk counter starts at zero and increments by one per chunk.
- `final_flag` is `0x00` for all non-final chunks and `0x01` for the final
  chunk.

Since `plaintext_size` < 2^64 and chunks are 65536 bytes, the counter never
exceeds 2^48; implementations MAY therefore use a 64-bit counter internally,
big-endian-encoded into the low-order 8 bytes of the 11-byte counter field
with the high-order 3 bytes zero.

A reader MUST verify each chunk's tag (by decrypting with the expected nonce
for that chunk index and finality) before releasing or hashing that chunk's
plaintext. The expected finality of each chunk is determined by the chunk
count (Section 8.2.3), never by inspecting the stream: a chunk that
authenticates only under the wrong finality fails authentication by
construction.

#### 8.2.3. Chunk Count and Frame Length

Let `P = plaintext_size` and `C = 65536`. The number of encrypted chunks is:

```text
chunk_count = max(1, ceil(P / C))
```

Implementations MUST compute this without intermediate overflow, for example
as `P / C + (1 if P mod C != 0 else 0)`, special-casing `P = 0` to 1.

The expected AEAD payload frame length and footer offset are:

```text
payload_frame_len = P + 16 * chunk_count
footer_offset     = 64 + metadata_frame_len + payload_frame_len
```

### 8.3. Arithmetic and Overflow

`plaintext_size` is an unsigned 64-bit quantity supplied by metadata; in raw
mode the metadata is attacker-controllable, and even in AEAD mode the value
is processed before any payload bytes are read. Readers and Restorers MUST
compute all derived quantities — `payload_frame_len`, `footer_offset`, chunk
offsets, and PFR ranges — using checked unsigned 64-bit arithmetic.

A `plaintext_size` for which any derived quantity exceeds 2^64 − 1 cannot
correspond to a real object. A reader that computes such a quantity eagerly
MUST reject the overflow with `InvalidMetadataField`. A reader that streams
chunk-by-chunk without materializing the totals never reaches the overflow;
it instead encounters the framing failure that the impossible size implies
(`UnexpectedEof` or `MissingFinalChunk`), which is equally conformant,
provided each incremental computation is itself checked. In all cases,
arithmetic MUST NOT silently wrap, and overflow MUST NOT cause a panic,
crash, or unbounded allocation (Section 15.2).

## 9. Completion Footer

Every complete AOF1 object MUST end with the 16-byte completion footer:

```text
ASCII("AOF1_STREAM_END.")
```

Hex:

```text
41 4f 46 31 5f 53 54 52 45 41 4d 5f 45 4e 44 2e
```

The footer is a mechanical completion marker, not a cryptographic integrity
mechanism. It detects incomplete writer success paths (Section 11.3) and
makes append-only backend behavior easier to reason about.

Readers MUST verify that the footer appears exactly at the derived
`footer_offset` (Sections 8.1, 8.2.3) and MUST reject a mismatch with
`InvalidFooter`. A byte sequence matching the footer inside the payload has
no special meaning, and readers MUST NOT locate the footer by scanning.

Integrity and corruption detection are provided by `stored_digest`
(Section 10), the AEAD tags in `aead-stream-v1`, and `plaintext_digest`
validation (Sections 11, 12).

## 10. Stored Digest

The `stored_digest` is not stored inside the AOF1 object. It is held by the
backend catalog or master index:

```text
stored_digest = SHA-256(complete AOF1 stored bytes)
```

where the complete stored bytes run from byte 0 of the header through the
final byte of the completion footer.

Backends MUST be able to scrub and verify AOF1 objects using `stored_digest`
without keys or plaintext access.

For `raw-v1`, a trusted external `stored_digest` is the primary tamper and
corruption check (Section 17.1). For `aead-stream-v1`, `stored_digest`
remains useful for backend scrubbing and byte-for-byte cross-backend
comparison, while the AEAD layer provides cryptographic authentication
during open and verify.

## 11. Sealing

### 11.1. Ordering Requirements

AOF1 is designed for single-pass sealing once the upstream payload size and
digest are known.

Writers MUST fully serialize the AOF1-CBOR metadata before constructing the
public header, because `metadata_frame_len` is a header field and, in AEAD
mode, the header hash is an input to key derivation (Section 7.1). In AEAD
mode, writers MUST derive keys from the hash of the final header. Writers
MUST NOT derive AEAD keys using a placeholder metadata length and then
backfill the header; a backfilled header would silently change `header_hash`
after keys were derived, producing an unreadable object.

### 11.2. Workflow

The normative writer obligations are steps 4–10; steps 1–3 describe the
expected division of labor with an orchestrator.

1. The orchestrator creates the canonical plaintext payload, normally an
   uncompressed tar stream or staged tar file.
2. During staging, the orchestrator computes `plaintext_size` and
   `plaintext_digest`.
3. The orchestrator passes the metadata to the writer.
4. The writer serializes and validates the AOF1-CBOR metadata
   (Sections 5.1–5.3).
5. The writer constructs the final public header using the known metadata
   frame length, and in AEAD mode generates the salt (Section 7.3) and
   derives keys from the final header hash (Section 7.1).
6. The writer writes the header, the metadata frame, and the payload frame,
   streaming the payload through encryption in AEAD mode.
7. While consuming the payload, the writer independently computes the
   plaintext size and SHA-256 digest of the bytes actually read.
8. If the computed size differs from the metadata `plaintext_size`, sealing
   MUST fail with `PlaintextSizeMismatch`; if the computed digest differs
   from `plaintext_digest`, sealing MUST fail with
   `PlaintextDigestMismatch`.
9. On any failure, the writer MUST NOT write the completion footer
   (Section 11.3).
10. On success, the writer writes the completion footer and reports the
    computed `stored_digest` and stored size to the caller.

Step 7 is mandatory even though the orchestrator already computed the same
values: it proves that the writer sealed the payload it was given, not the
payload the metadata describes.

### 11.3. Failure Handling

An object missing its footer is incomplete by definition and MUST NOT be
treated as successfully sealed, regardless of how many bytes were written.
Writers MUST report failure to the caller whenever the footer was not
written. Partial outputs SHOULD be deleted or quarantined; they MUST NOT be
referenced by any durable catalog.

### 11.4. Durable Commitment

On seekable filesystems, writers SHOULD write to a temporary path (for
example, `name.aof.partial`) and rename to the final path only after
success. Rename-based commitment is meaningful only if the data is durable
before the rename: writers SHOULD flush and synchronize the file contents to
stable storage (for example, `fsync`) before the rename, and SHOULD
synchronize the containing directory afterward, before reporting success to
the orchestrator. A rename without prior synchronization can leave the
final name referring to incompletely persisted data after a crash, which
defeats the purpose of the commitment protocol.

Temporary paths SHOULD be created exclusively (failing if the path already
exists) so that concurrent writers targeting the same output cannot corrupt
each other's partial output.

On append-only media such as tape, the storage backend MUST treat the
written extent as pending until the writer reports success and the footer
and `stored_digest` have been verified. Failed pending extents MUST NOT be
referenced by the durable catalog.

## 12. Opening and Verification

### 12.1. Reader Procedure

A Reader or Verifier processing a whole object MUST:

1. Read exactly 64 header bytes and validate them (Section 4.5).
2. Read exactly `metadata_frame_len` bytes.
3. Decode the metadata:
   - In `raw-v1`, validate and parse the AOF1-CBOR plaintext directly.
   - In `aead-stream-v1`, resolve the root key (Section 6), derive keys
     (Section 7.1), authenticate and decrypt the metadata frame, then
     validate and parse the AOF1-CBOR plaintext.
4. Validate the metadata schema (Section 5.3).
5. Derive the exact payload frame length from `plaintext_size`
   (Section 8), rejecting overflow per Section 8.3.
6. Process the payload frame:
   - In `raw-v1`, read exactly `plaintext_size` bytes.
   - In `aead-stream-v1`, authenticate and decrypt exactly the expected
     chunks (Section 8.2), requiring an authenticated final chunk.
7. Verify the plaintext digest and size (Section 12.2).
8. Verify the completion footer at the derived `footer_offset`
   (Section 9). Steps 7 and 8 both follow payload processing and MAY be
   performed in either order.
9. If the input is a whole-object input, confirm EOF (Section 3.3).

A failure at any step MUST abort processing with the corresponding typed
error (Section 14).

### 12.2. Plaintext Digest Verification

Readers and Verifiers MUST compute SHA-256 over the recovered plaintext
payload while processing it, MUST compare the computed size and digest with
the metadata `plaintext_size` and `plaintext_digest`, and MUST fail with
`PlaintextSizeMismatch` or `PlaintextDigestMismatch` on disagreement.

This requirement is absolute in raw mode, where the digest comparison is the
only payload-corruption check that exists without an external catalog. It
also applies in AEAD mode: although chunk authentication already guarantees
payload integrity under the derived key, the digest check additionally
proves that the *sealer* matched metadata to payload, and it detects
key-holder forgeries in which authenticated metadata deliberately misstates
the digest (Section 14, note on encrypted-mode forgeries).

Backend verification SHOULD additionally compute `stored_digest` over the
complete object bytes and compare it with the trusted catalog value
(Section 10).

### 12.3. Keyless Verification of Encrypted Objects

A Keyless Verifier processing an `aead-stream-v1` object without key
material:

- MUST validate the public header (Section 4.5);
- MUST read the declared metadata frame, failing with `UnexpectedEof` if it
  is incomplete;
- MUST compute `stored_digest` over all input bytes consumed;
- CANNOT verify the footer *position*, because `plaintext_size` is inside
  the encrypted metadata; but
- SHOULD verify that the input *ends with* the 16 footer bytes, reporting
  this as a distinct, weaker observation than positional footer
  verification. A missing terminal footer indicates an incomplete writer
  even when no key is available.

A Keyless Verifier MUST NOT claim authentication. Its `stored_digest` output
is meaningful only in comparison with a trusted catalog value.

### 12.4. Streamed Output Hazards

Open is a streaming operation: plaintext is necessarily emitted before the
final size, digest, footer, and EOF checks complete. Consumers MUST NOT
treat streamed output as complete or valid until the open operation reports
overall success.

In `aead-stream-v1`, every emitted byte has already been authenticated at
chunk granularity, so a failure mid-stream still leaves the consumer with an
authentic prefix of the true plaintext. In `raw-v1` no such guarantee
exists: bytes emitted before a failure are unverified. Implementations that
write open output to a file SHOULD apply the same temporary-path commitment
protocol as sealing (Section 11.4).

## 13. Partial File Restore

### 13.1. Index Requirements

Partial File Restore (PFR) indexes MUST use plaintext payload offsets as the
source of truth. The master index SHOULD store, for each restorable file
member:

```text
plaintext_payload_offset
plaintext_length
optional member plaintext digest
tar header offset or application metadata
```

The index MUST NOT make ciphertext chunk boundaries canonical. Ciphertext
offsets are derived implementation details, reproducible from this section.

### 13.2. Range Validation

A Restorer MUST validate every requested range before applying any mapping
formula, using checked arithmetic (Section 8.3):

1. If `plaintext_length = 0`, the result is an empty range set. No payload
   bytes need to be fetched; the restore is a metadata or header
   reconstruction operation, and the formulas below are not applied.
2. Otherwise the range MUST satisfy
   `plaintext_payload_offset + plaintext_length <= plaintext_size`, where
   the sum MUST be computed without overflow. A Restorer MUST reject ranges
   that violate this bound.

The formulas in Sections 13.3 and 13.4 are defined only for validated
ranges with `plaintext_length > 0`; their behavior for out-of-range inputs
is deliberately undefined, which is why validation is mandatory.

### 13.3. Raw Mapping

For `raw-v1`:

```text
payload_start = 64 + metadata_frame_len
stored_offset = payload_start + plaintext_payload_offset
stored_length = plaintext_length
```

### 13.4. AEAD Mapping

For `aead-stream-v1`, with `C = 65536` and `chunk_count` as in
Section 8.2.3:

```text
payload_cipher_start = 64 + metadata_frame_len

start_chunk = floor(plaintext_payload_offset / C)
end_chunk   = floor((plaintext_payload_offset + plaintext_length - 1) / C)
```

For each chunk index `i` in `start_chunk ..= end_chunk`:

```text
chunk_plain_offset  = i * C
chunk_plain_len     = C                        if i <  chunk_count - 1
                    = plaintext_size - (i * C) if i == chunk_count - 1
chunk_cipher_offset = payload_cipher_start + i * (C + 16)
chunk_cipher_len    = chunk_plain_len + 16
```

For a validated range, `end_chunk < chunk_count` always holds; a Restorer
SHOULD nevertheless verify it as a defense-in-depth check.

The restore process fetches each complete encrypted chunk, authenticates and
decrypts it, and slices the requested plaintext bytes from the decrypted
chunks. The nonce for chunk `i` is constructed per Section 8.2.2 with the
counter equal to `i` and:

```text
final_flag = 0x01 if i == chunk_count - 1, else 0x00
```

The finality of a fetched chunk is determined by this formula alone — that
is, from the authenticated `plaintext_size` — never by probing both flag
values or by position relative to EOF. A Restorer MUST NOT release plaintext
from a chunk whose tag failed to verify.

### 13.5. Compression Rule

AOF1 itself performs no compression.

If the payload is an archive intended for arithmetic PFR, it SHOULD be an
uncompressed tar stream or another format whose member byte ranges are
stable and independently addressable. Whole-stream compression such as
`.tar.gz` or `.tar.zst` destroys simple PFR because restoring a later member
generally requires decompressing earlier bytes.

File-level compression inside the archive is acceptable. Media formats such
as ProRes, H.264, H.265, RAW image formats, and other already-compressed
assets may be stored as ordinary tar members.

## 14. Errors

### 14.1. Error Taxonomy

Implementations SHOULD expose typed errors equivalent to the following. The
names are normative for the test-vector manifest (Section 20); the surface
syntax is not.

```text
InvalidMagicBytes            input does not begin with AOF1
InvalidHeaderLength          header_len field is not 64
ReservedBytesNotZero         flags or reserved bytes are nonzero
UnsupportedVersion           reserved; see note below
InvalidRepresentation        unknown representation value
InvalidModeSuiteCombination  unknown suite value, or mode/suite/chunk_size invariant violated
InvalidKeyIdentifier         all-zero key_id in AEAD mode, or key_id unknown to the resolver
InvalidRootKey               root key material shorter than 32 bytes
InvalidSalt                  all-zero hkdf_salt in AEAD mode
MetadataFrameLengthInvalid   metadata_frame_len outside its bounds
UnexpectedEof                declared header, metadata, raw payload, or footer bytes missing
MissingFinalChunk            EOF within the AEAD payload frame before an authenticated final chunk
InvalidFooter                bytes at footer_offset are not the footer
TrailingData                 bytes present after the footer in a whole-object input
AeadAuthenticationFailed     metadata or chunk tag verification failed
InvalidCborEncoding          metadata is not valid AOF1-CBOR (Sections 5.1, 5.2)
MissingRequiredMetadataField a required schema key is absent
InvalidMetadataField         a schema key has the wrong type, value, or implies overflow
PlaintextDigestMismatch      computed payload digest differs from plaintext_digest
PlaintextSizeMismatch        computed payload size differs from plaintext_size
RandomSourceFailure          (writer-side, OPTIONAL) the CSPRNG failed
Io                           an underlying I/O failure that is not a format violation
```

`UnsupportedVersion` is reserved for future AOF-family format handling. In
AOF1, the magic bytes are the version marker, and a reader encountering any
non-`AOF1` magic reports `InvalidMagicBytes`. If an implementation later
recognizes an AOF-family magic (such as `AOF2`) that it cannot process, it
MAY report `UnsupportedVersion` instead. This variant is intentionally
unused by pure-AOF1 implementations; it MUST NOT be removed from the
taxonomy on that basis.

Implementations SHOULD keep I/O failures (`Io`) distinct from format
violations so that callers can distinguish storage problems from invalid
objects.

For encrypted mode, a forged `plaintext_digest` or `plaintext_size` can be
created only by a writer holding encryption authority for the object's key,
because those fields are inside the authenticated metadata frame. The
mismatch checks of Sections 11.2 and 12.2 are required regardless: at seal
time they prove the writer validated injected metadata against the actual
payload, and at open time they detect key-holder forgeries and raw-mode
corruption.

### 14.2. Error Condition Mapping

The following mapping is normative for single-fault inputs and is exercised
by the negative test vectors (Section 20.2).

| Condition | Error |
| --- | --- |
| Magic ≠ `AOF1` | `InvalidMagicBytes` |
| `header_len` ≠ 64 | `InvalidHeaderLength` |
| Unknown `representation` value | `InvalidRepresentation` |
| Unknown `suite_id` value | `InvalidModeSuiteCombination` |
| `flags` ≠ 0 or `reserved` ≠ 0 | `ReservedBytesNotZero` |
| Mode/suite/`chunk_size` invariant violated (Section 4.3) | `InvalidModeSuiteCombination` |
| AEAD header with all-zero `key_id` | `InvalidKeyIdentifier` |
| AEAD header with all-zero `hkdf_salt` | `InvalidSalt` |
| `metadata_frame_len` > 16 MiB, or below the per-mode minimum | `MetadataFrameLengthInvalid` |
| EOF while reading header, metadata frame, raw payload, or footer bytes | `UnexpectedEof` |
| EOF within the AEAD payload frame (mid-chunk or between chunks) | `MissingFinalChunk` |
| Metadata or chunk tag failure, including header bit flips and wrong final flag | `AeadAuthenticationFailed` |
| Metadata not valid AOF1-CBOR: noncanonical encoding, forbidden item, duplicate or unsorted keys, trailing bytes, depth or item-count limit exceeded, top-level not a map of unsigned keys | `InvalidCborEncoding` |
| Required metadata key absent | `MissingRequiredMetadataField` |
| Metadata key with wrong type or value; `metadata_version` ≠ 1; digest algorithm ≠ `sha256`; digest length ≠ 32; `plaintext_size` implying overflow when totals are computed eagerly (Section 8.3) | `InvalidMetadataField` |
| Footer bytes wrong at `footer_offset` | `InvalidFooter` |
| Bytes after the footer in a whole-object input | `TrailingData` |
| Sealed or recovered payload size disagrees with metadata | `PlaintextSizeMismatch` |
| Sealed or recovered payload digest disagrees with metadata | `PlaintextDigestMismatch` |
| Root key shorter than 32 bytes | `InvalidRootKey` |
| `key_id` not resolvable to key material | `InvalidKeyIdentifier` |

### 14.3. Multiple Faults

When an input contains several faults, the reported error depends on
processing order. Implementations SHOULD follow the order of Sections 4.5
and 12.1; for inputs that remain multi-faulted within a single step, any
applicable error is conformant. Test vectors avoid this ambiguity by
containing exactly one fault each.

## 15. Implementation Requirements

### 15.1. Core Library

The reference implementation project is named `amber`; its core crate SHOULD
be named `amber_core` and SHOULD expose synchronous APIs over byte-stream
abstractions, for example:

```rust
seal<R: std::io::Read, W: std::io::Write>(input: R, output: W, options: SealOptions)
open<R: std::io::Read, W: std::io::Write>(input: R, output: W, options: OpenOptions)
verify<R: std::io::Read>(input: R, options: VerifyOptions)
```

The core crate MUST NOT depend on an orchestrator, a storage backend, a tape
library, a network service, KMIP, or an async runtime, and MUST NOT perform
network calls. Key material retrieval belongs to the caller: the core crate
accepts root keys through an in-memory interface (for example, a resolver
trait keyed by `key_id`).

"No architectural dependency" does not mean no software dependencies. The
implementation SHOULD use well-known, maintained libraries for HKDF,
SHA-256, ChaCha20-Poly1305, and constant-time comparison.

### 15.2. Panic Safety

The implementation MUST NOT panic, crash, abort, or invoke undefined
behavior on malformed or hostile input. Unchecked assertions (`unwrap`,
`expect`), unchecked indexing, and unchecked arithmetic are forbidden on all
code paths reachable from:

- AOF1 header bytes;
- metadata frame bytes;
- payload frame bytes;
- footer bytes; and
- injected sealing metadata supplied by an orchestrator.

Panics in tests and for static internal invariants are permitted, but they
MUST NOT be reachable from malformed input. Implementations SHOULD enforce
this mechanically (for example, by forbidding unsafe code crate-wide and
enabling lints against unchecked assertions outside test code) and SHOULD
validate it empirically with coverage-guided fuzzing of the header,
metadata, and whole-object parsers (Section 21).

### 15.3. Command-Line Tools and Key Handling

A CLI built on the core library is responsible for argument parsing, file
and stream plumbing, receiving root keys, invoking the core, printing
diagnostics to stderr, and exiting nonzero on any error. The reference CLI
exposes `create`, `inspect`, `verify`, and `open`.

Key material MUST NOT be passed on command lines: argv is commonly visible
through process listings and shell history. Keys SHOULD be provided through
a key agent, protected file descriptor, local IPC mechanism, or equivalent
secure channel. A development-only key-file path is acceptable provided the
byte interpretation is explicit (for example, `--key-format raw|hex`); a
tool MUST NOT guess whether a key file contains raw bytes or hex text.

CLI output writers SHOULD follow the durable commitment protocol of
Section 11.4 for both `create` and `open`.

## 16. Storage Backend Contract

Storage backends treat AOF1 objects as opaque bytes. A backend SHOULD store,
per object:

```text
object location
representation mode
stored_digest
byte extents
stripe or parity membership
backend-local catalog information
```

The backend MUST NOT need plaintext payload bytes, plaintext metadata, or
object-encryption internals to perform durability, parity, and scrub
operations. Parity, where applicable, is computed over stored AOF1 bytes,
not over plaintext payload bytes.

Backend-local catalogs MAY themselves be stored as AOF1 objects — for
example, `raw-v1` on a plaintext cartridge and `aead-stream-v1` on an
encrypted cartridge. This reuse is optional and does not change the
contract: catalog objects, like asset objects, are stored as opaque bytes
and verified by `stored_digest`.

## 17. Security Considerations

### 17.1. Raw Mode Provides No Self-Authentication

`raw-v1` is not confidential and is not tamper-resistant by itself. An
attacker who can rewrite a raw object can also rewrite the unkeyed SHA-256
claims inside it. Raw-mode integrity therefore depends on a trusted
external `stored_digest` held by the backend catalog or master index.

A `raw-v1` copy provides availability without access to AEAD root keys or an
HSM, because its payload is plaintext. That availability property does not
make the object self-authenticating: a lone raw object without a trusted
external `stored_digest` is not a sufficient integrity proof.

### 17.2. AEAD Mode Assumptions

`aead-stream-v1` provides confidentiality and authentication for metadata
and payload bytes under these assumptions:

- root key material remains secret and contains the required entropy
  (Section 7.2);
- `hkdf_salt` is fresh per object (Section 7.3);
- the implementation follows the nonce, key-separation, and final-chunk
  rules exactly (Sections 5.5, 7.4, 8.2); and
- the key registry maps `key_id` values correctly and preserves old key
  epochs (Section 6).

### 17.3. Salt Uniqueness Is the Single Point of Failure

The public header intentionally contains no payload-dependent entropy
(Section 17.5), so the derived keys of two objects sealed under the same
root key differ only insofar as their headers differ — and the only
guaranteed-distinct header field is the random `hkdf_salt`.

If a writer reuses a salt under one root key, two objects with otherwise
identical header fields (same `key_id`, same `metadata_frame_len`, which is
common under a fixed metadata schema) derive byte-identical `metadata_key`
and `payload_key`. The zero metadata nonce is then reused across distinct
metadata plaintexts, and each payload chunk index reuses its nonce across
distinct chunk plaintexts. For ChaCha20-Poly1305 this is a total failure:
XOR of plaintexts leaks immediately, and Poly1305 key recovery enables
forgery. Note that the failure does not require identical payloads — any
two objects under the reused salt are affected.

This is why Section 7.3 states salt freshness as an absolute requirement and
why fixed-salt interfaces must be fenced off from production use. With
fresh 16-byte random salts, accidental collision requires on the order of
2^64 objects under one root key (birthday bound), which is far beyond any
plausible archive scale.

### 17.4. Key Separation Is Load-Bearing

The metadata AEAD nonce (12 zero bytes) is byte-identical to the nonce of a
non-final payload chunk 0. The construction is safe only because
`metadata_key` ≠ `payload_key` (Section 7.4). Any change that unifies the
two keys converts this nonce coincidence into a real nonce-reuse
vulnerability. This invariant is restated here so that no future revision
or "simplification" removes it.

### 17.5. Header Privacy and Size Leakage

The public header reveals: that the object is AOF1; whether it is raw or
encrypted; the cryptographic suite; the key epoch identifier for encrypted
objects; and the metadata frame length. It MUST NOT contain filenames,
asset identifiers, plaintext digests, project names, policy tags, or other
sensitive content. Such information belongs in the metadata, which is
encrypted in AEAD mode.

Independent of header contents, `aead-stream-v1` necessarily reveals sizes:
the metadata frame length is public, and the exact `plaintext_size` is
derivable from the payload frame length (Section 8.2.3). Deployments for
which payload size is itself sensitive must apply padding at the payload
layer before sealing; AOF1 defines no padding mechanism.

### 17.6. AEAD Key Commitment

ChaCha20-Poly1305 is not a key-committing AEAD. A party holding two root
keys (for example, two epochs in the same registry) can in principle craft a
single ciphertext that authenticates and decrypts to different valid
plaintexts under each key [PART-ORACLE] [AEAD-COMMIT]. `stored_digest` does
not prevent this: equivocation uses one byte string. AOF1's defense is
operational, not cryptographic: the key registry is trusted, `key_id`
resolution is exact, and writers are within the trust boundary
(Section 14.1, note on forgeries). Deployments in which writers are
mutually distrusting, or in which one object must never be interpretable
under two epochs, need a committing construction and therefore a future
format revision.

### 17.7. Root Key Rotation

AOF1 AEAD mode uses derived per-object keys and stores no wrapped data
encryption keys in the object. Consequences:

- old root key epochs must remain recoverable for as long as objects
  encrypted under them must remain readable;
- rotating root keys affects newly created objects only; and
- rewrapping without rewriting payload bytes is not supported by AOF1.

### 17.8. Algorithm Agility

AOF1 intentionally rejects unknown header values and unsupported suites. If
a future cryptographic suite, compression layer, metadata interpretation, or
payload representation is needed, define a new format under a new magic
(`AOF2`). This is a deliberate trade: no in-place agility, in exchange for
a format whose every valid object is parseable by every conformant reader
forever.

### 17.9. Resource Consumption

All reader allocations are bounded by format rules: the metadata frame is
capped at 16 MiB (Section 4.4); decoded metadata is additionally capped by
the nesting-depth and item-count limits of Section 5.2, which bound decode
memory to a small multiple of the frame size; and payload processing
requires only constant memory per chunk. Readers MUST NOT allocate based on
unvalidated declared lengths (for example, a CBOR length argument exceeding
the remaining input, or a `plaintext_size` used for anything other than
incremental streaming).

Services that verify untrusted objects SHOULD additionally apply external
resource limits (time, memory, concurrency) as defense in depth.

### 17.10. Output Handling During Open

See Section 12.4: streamed plaintext is not trustworthy until open reports
success, raw-mode output is unverified until the final digest comparison,
and file outputs should use the same commitment protocol as sealing.

## 18. Operational Considerations (Informative)

AOF1 sealing consumes CPU, memory bandwidth, and storage bandwidth. The
orchestrator, not the format implementation, should enforce resource
scheduling: limit concurrent sealing workers, prefer job-level parallelism
over internal per-object threading, and use OS scheduling controls (`nice`,
`ionice`, cgroups, or equivalents) to keep sealing from starving
ingest and transcoding workloads. The core implementation should remain
deterministic and single-stream; scheduling policy does not belong in the
format crate.

## 19. IANA Considerations

This document requires no IANA actions. AOF1 is identified in-band by its
magic bytes and out-of-band by the `.aof` file extension. If AOF1 is ever
published for public interchange, a media type registration (for example,
`application/vnd.aof1`) should accompany it; until then, implementations
should treat the media type as unregistered.

## 20. Test Vector Requirements

AOF1 cannot be frozen until a reference implementation and static test
vectors exist (Section 21). Vector manifests record expected error names
using the taxonomy of Section 14.1.

### 20.1. Positive Vectors

The suite MUST include, for both `raw-v1` and `aead-stream-v1`, objects with
payload sizes:

```text
0, 1, 65535, 65536, 65537, 131072
```

For each positive case the manifest MUST pin, at minimum:

- the exact metadata plaintext bytes (AOF1-CBOR);
- the exact 64 header bytes and the header hash;
- for AEAD cases: the root key, `key_id`, salt, derived `metadata_key` and
  `payload_key`, and the exact metadata frame bytes;
- the payload frame, either as exact bytes or as a SHA-256 digest of the
  payload frame; cases with `plaintext_size` ≤ 1 MUST include exact bytes;
- the stored size and the exact full-object `stored_digest`.

At least one positive case MUST include a canonical metadata map carrying an
unknown descriptive top-level unsigned integer key outside the defined set;
this object MUST decode, verify, and open successfully (Section 5.3.3).

### 20.2. Negative Vectors

The suite MUST include negative vectors for at least the following, each
containing exactly one fault and asserting the mapped error of
Section 14.2:

Header faults:

1. Invalid magic bytes.
2. `header_len` ≠ 64.
3. Nonzero `flags`.
4. Nonzero `reserved` bytes.
5. Unknown `representation` value.
6. Invalid raw mode/suite combination.
7. Invalid AEAD mode/suite combination.
8. AEAD header with all-zero `key_id`.
9. AEAD header with all-zero `hkdf_salt`.
10. `metadata_frame_len` exceeding 16 MiB.

Cryptographic binding faults:

11. AEAD header bit flip in a structurally valid bound field such as
    `hkdf_salt`, failing with `AeadAuthenticationFailed`.
12. AEAD header `key_id` changed to another known test key, failing with
    `AeadAuthenticationFailed`. (A flip to an unknown `key_id` may instead
    fail earlier, during key resolution.)
13. Wrong final flag on a payload chunk.
14. An AEAD object sealed with a known test key whose authenticated
    metadata deliberately misstates `plaintext_digest`; open and keyed
    verify MUST fail with `PlaintextDigestMismatch` (Section 12.2).

Framing faults:

15. Unexpected EOF while reading the metadata frame.
16. Truncated payload mid-chunk.
17. Missing authenticated final chunk (payload absent after metadata).
18. Invalid footer bytes at the correct offset.
19. Trailing data after the footer.

Metadata faults:

20. Noncanonical or out-of-repertoire CBOR — at minimum: a non-shortest
    integer encoding, a duplicate map key, an indefinite-length item, a
    floating-point value, and a tagged item.
21. Nesting depth exceeding `AOF1_MAX_CBOR_NESTING_DEPTH`.
22. Item count exceeding `AOF1_MAX_METADATA_ITEMS`.
23. Missing required metadata field.
24. `metadata_version` ≠ 1.

Seal-time faults (writer conformance):

25. Injected metadata with wrong `plaintext_digest`; sealing MUST fail with
    `PlaintextDigestMismatch` and MUST NOT write a footer.
26. Injected metadata with wrong `plaintext_size`; sealing MUST fail with
    `PlaintextSizeMismatch` and MUST NOT write a footer.

## 21. Candidate Freeze Criteria

AOF1 remains a candidate specification until all of the following hold:

1. The reference implementation implements this document.
2. Static positive and negative vectors exist covering every case in
   Section 20, and the reference implementation passes them.
3. At least one independent verification tool can parse headers and verify
   `stored_digest` from this document alone.
4. Raw and AEAD objects round-trip at all required boundary sizes.
5. PFR mapping is tested against AEAD objects by fetching mapped ciphertext
   ranges, authenticating and decrypting them, and comparing the sliced
   plaintext to the original payload bytes — not merely by checking range
   arithmetic.
6. A failed sealing operation is proven not to produce a committed object.
7. Coverage-guided fuzzing of the header parser, the AOF1-CBOR decoder, and
   the whole-object open/verify paths has run to a meaningful corpus
   plateau with no panics, crashes, or hangs, supporting the Section 15.2
   guarantee empirically.

After these conditions are met, the specification may be marked frozen as
AOF1. Once frozen, no normative change to this document is permitted other
than errata that do not change the set of valid objects; all other changes
require a new magic (Section 17.8).

## 22. References

### 22.1. Normative References

- [RFC2119] Bradner, S., "Key words for use in RFCs to Indicate Requirement
  Levels", BCP 14, RFC 2119.
- [RFC8174] Leiba, B., "Ambiguity of Uppercase vs Lowercase in RFC 2119 Key
  Words", BCP 14, RFC 8174.
- [RFC5869] Krawczyk, H. and P. Eronen, "HMAC-based Extract-and-Expand Key
  Derivation Function (HKDF)", RFC 5869.
- [RFC8439] Nir, Y. and A. Langley, "ChaCha20 and Poly1305 for IETF
  Protocols", RFC 8439.
- [RFC8949] Bormann, C. and P. Hoffman, "Concise Binary Object
  Representation (CBOR)", STD 94, RFC 8949.

### 22.2. Informative References

- [AGE] "The age format specification", C2SP. Payload STREAM construction
  reference.
- [PART-ORACLE] Len, J., Grubbs, P., and T. Ristenpart, "Partitioning
  Oracle Attacks", USENIX Security 2021.
- [AEAD-COMMIT] Albertini, A., Duong, T., Gueron, S., Kölbl, S., Luykx, A.,
  and S. Schmieg, "How to Abuse and Fix Authenticated Encryption Without
  Key Commitment", USENIX Security 2022.

---

## Appendix A. Changes from Revision 1 (Informative)

Revision 2 incorporates the findings of the 2026-06-10 specification review
(`docs/code_review_2026-06-10.md` records the companion implementation
review). No cryptographic construction changed; every wire-format byte of a
Revision 1 object that satisfied the new requirements remains valid.

### A.1. New Normative Requirements

1. **Salt freshness (Section 7.3).** Per-object fresh CSPRNG salt is now an
   explicit MUST with an explicit MUST NOT on reuse; previously freshness
   was only implied by Security Considerations. Fixed-salt interfaces must
   be documented as unsafe and separated from normal sealing.
2. **AOF1-CBOR repertoire (Section 5.1).** The accepted CBOR subset is now
   enumerated normatively. Tags, `undefined`, one-byte simple values, all
   simple values other than false/true/null, floats, and indefinite-length
   items are explicitly rejected; duplicate-key rejection and UTF-8
   validity are explicit; nested map key typing is defined.
3. **Structural metadata limits (Section 5.2).** New format-level limits:
   nesting depth ≤ 32 and total item count ≤ 65536, bounding decode memory
   amplification. (Revision 1 had no limits; the reference implementation
   had a private depth limit of 128, which now tightens to the normative
   32 and gains an item-count check.)
4. **Trailing data (Sections 3.3, 14).** Whole-object inputs must end at
   the footer; violations report the new `TrailingData` error. Embedding
   contexts must delimit objects externally. (The reference implementation
   previously detected this but reported `InvalidFooter`.)
5. **Mandatory plaintext digest verification on open/verify
   (Section 12.2).** Previously a SHOULD attached only to "full
   verification"; now a MUST for Readers and Verifiers in both modes,
   matching existing reference-implementation behavior.
6. **PFR range validation and finality (Section 13).** Range bounds
   checking is now mandatory before applying mapping formulas; the
   final-flag determination for fetched chunks (`i == chunk_count − 1`) is
   now specified; the dead `plaintext_size = 0` branch of the Revision 1
   chunk-length formula is removed (zero-length ranges exit before the
   formulas apply).
7. **Root key length (Section 7.2).** "At least 32 bytes of entropy"
   (unenforceable) is replaced by an enforceable MUST-reject below 32
   bytes plus a RECOMMENDED generation policy, with the new
   `InvalidRootKey` error.
8. **Overflow handling (Section 8.3).** Checked 64-bit arithmetic is
   mandatory for all derived quantities; metadata implying overflow is
   rejected with `InvalidMetadataField`.
9. **Durable commitment (Section 11.4).** Rename-based commitment now
   carries fsync-before-rename and exclusive-creation guidance.
10. **Keyless footer suffix check (Section 12.3).** Keyless Verifiers
    SHOULD confirm the input ends with the footer bytes.

### A.2. Clarifications and Additions

11. **Validation order and multi-fault inputs (Sections 4.5, 14.3).** A
    recommended order is defined; any applicable error is conformant for
    multi-fault inputs; test vectors are single-fault by construction.
12. **Error condition mapping table (Section 14.2)** — previously implied
    by scattered prose and the vector list. This also resolves Revision 1's
    ambiguous classification of payload EOF, which listed "chunk" and
    "payload" under both `UnexpectedEof` and `MissingFinalChunk`: raw
    payload EOF is `UnexpectedEof`; AEAD payload EOF is
    `MissingFinalChunk`, matching reference-implementation behavior.
13. **`UnsupportedVersion` rationale (Section 14.1)** moved into the
    specification from the implementation plan, with an explicit note that
    the variant is intentionally unused in AOF1.
14. **Frozen-field rationale (Section 4.2)** for `header_len`,
    `chunk_size`, and `suite_id`: self-description and damage detection,
    not agility.
15. **Key-separation invariant (Sections 5.5, 7.4, 17.4)**: the metadata
    zero nonce coincides with the payload chunk-0 non-final nonce and is
    safe only because the keys are distinct; merging the keys is
    prohibited.
16. **Security Considerations expanded**: salt-reuse consequences and
    birthday bound (17.3), size leakage (17.5), non-committing AEAD and
    cross-epoch equivocation (17.6), resource consumption (17.9), and
    streamed-output hazards (12.4, 17.10).
17. **Empty AAD rationale (Section 5.5)**: header binding is achieved
    through key derivation.
18. **Conformance targets (Section 2.2)**, constants table (Section 2.5),
    BCP 14 boilerplate, IANA considerations, and split
    normative/informative references added for specification hygiene.
19. **Test vectors (Section 20)**: added required negative vectors for
    all-zero AEAD `key_id`, all-zero AEAD salt, unknown representation
    value, trailing data, CBOR repertoire violations (duplicate key,
    indefinite length, float, tag), depth and item-count limits, wrong
    `metadata_version`, and an open-side authenticated-metadata digest
    forgery; the positive unknown-descriptive-key vector moved here from
    the implementation plan; payload-frame pinning wording now matches
    practice (exact bytes or digest, exact bytes for sizes ≤ 1).
20. **Freeze criteria (Section 21)**: PFR criterion now requires actual
    fetch-decrypt-slice testing; a fuzzing criterion is added; a
    post-freeze change policy is stated.
21. **Resource scheduling** moved to an explicitly informative section
    (Section 18).

### A.3. Reference Implementation Impact

When Revision 2 was first published, the following requirements were not
yet met by the reference implementation: the `TrailingData` error variant;
the nesting-depth limit of 32 and the new item-count limit; PFR raw-range
bounds validation; the keyless footer suffix check; the open-side
digest-forgery and other new negative test vectors; the unsafe-use
documentation and segregation of the fixed-salt sealing path
(Section 7.3); and the durable-commitment (fsync, exclusive create)
behavior in the CLI.

As of 2026-06-10 the reference implementation implements all of the above.
In addition, post-implementation review hardened two paths beyond the
original list: the Section 5.2 structural limits are now enforced on the
encoding (writer) path as well as the decoding path, so a writer cannot
seal metadata that every conformant reader must reject and cannot be
crashed by deeply nested injected metadata (Section 15.2); and the decoder
bounds container pre-allocation by the item budget, so a declared container
length can never reserve more memory than the budget would allow parsing
(Section 17.9).
