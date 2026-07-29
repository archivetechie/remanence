# REM-ENCRYPT 1.0

## 1. Introduction

### 1.1. Identifiers and Versioning

| Identifier | Value | Scope |
| --- | --- | --- |
| Document version | 1.0 | This publication only |
| Concept DOI (all versions) | [10.5281/zenodo.21551570](https://doi.org/10.5281/zenodo.21551570) | This publication |
| Version DOI (this release) | [10.5281/zenodo.21551571](https://doi.org/10.5281/zenodo.21551571) | This publication |
| Envelope magic | `REMO` | Frozen four-byte wire constant |
| Key-frame magic | `REMK` | Frozen four-byte wire constant |
| On-tape `format_version` | `2` | Encrypted-envelope wire discriminator |
| `suite_id` | `0x01` | HKDF-SHA-256 + ChaCha20-Poly1305 |
| `wrap_suite` | `0x02` | X-Wing draft-10 |
| Stream format identifier | `rem-object-v1` | Canonical plaintext object defined by REM-OBJECT Core Format 1.0 |
| Default file extension | `.rem-object` | Both stored representations |

This document's version does not name any on-tape value. In particular,
document version 1.0 is independent of the frozen on-tape
`format_version = 2`; value `1` remains reserved and forbidden. The magic,
wire discriminators, and derivation labels in this document are wire
constants, not document-version indicators.

REM-ENCRYPT and REM-OBJECT Core share one section skeleton, so the two can be
read side by side: where a top-level number is absent from one document, the
other owns it. This document omits Sections 4, 9, and 14 (the plaintext
representation, the parity relationship, and conformance, owned by Core); Core
omits Section 5 (the encrypted representation, owned here).

### 1.2. Status of This Document

| | |
| --- | --- |
| Status | Publication specification |
| Version | 1.0 |
| Date | 2026-07-25 |
| License | CC-BY-4.0 |
| Concept DOI (all versions) | [10.5281/zenodo.21551570](https://doi.org/10.5281/zenodo.21551570) |
| Version DOI (this release) | [10.5281/zenodo.21551571](https://doi.org/10.5281/zenodo.21551571) |

This document is the publication specification for REM-ENCRYPT. It is the
normative fixed point for the encrypted representation it defines: an
implementation is validated against this document, not the reverse.

REM-ENCRYPT depends normatively on REM-OBJECT Core Format 1.0
([REMOBJECT]), which defines the canonical plaintext object sealed by this
profile. Its tape binding also depends on REM-PARITY 1.0 ([REMPARITY]).

### 1.3. Purpose and Boundary

REM-ENCRYPT defines an authenticated, confidential wrapper around the
canonical plaintext object specified by REM-OBJECT Core Format 1.0. It owns
the encrypted-envelope framing, public header, recipient key frame, object-key
schedule, encrypted metadata, chunk encryption, encrypted range mapping,
opening rules, error taxonomy, security analysis, and envelope vectors.

REM-ENCRYPT does not redefine the inner object, its manifest, paths, file
digests, or conformance roles. Those remain owned by [REMOBJECT]. An
implementation of encrypted objects needs both specifications; an
implementation that uses only plaintext objects does not need this one.

### 1.4. Design Goals

1. Seal the exact canonical REM-OBJECT byte string without changing its
   logical identity.
2. Hide member names, sizes, count, manifest content, and payload bytes.
3. Preserve closed-form partial-file restore.
4. Permit keyless storage, replication, parity repair, and stored-byte
   scrubbing.
5. Permit catalogless recovery from the object and one matching recipient
   private key.
6. Keep cryptographic evolution independent of the durable object format.

### 1.5. Non-Goals

REM-ENCRYPT defines no key registry, custody protocol, writer identity,
signature scheme, provenance mechanism, catalog format, or payload-padding
policy. It does not support an unencrypted envelope or in-place key-frame
rewriting. Resealing is a full read, open, and seal operation.

## 2. Conventions and Terminology

### 2.1. Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174]
when, and only when, they appear in all capitals, as shown here.

### 2.2. Conformance Roles

- **Sealer**: produces a REM-ENCRYPT envelope from a canonical plaintext
  object and one through eight recipient public keys.
- **Keyed Reader**: opens and verifies an envelope using a matching recipient
  private key, then passes the recovered stream to a Core Reader.
- **Keyless Verifier**: validates public envelope structure and computes
  `stored_digest` without claiming authenticity.
- **Encrypted Restorer**: maps authenticated inner-block ranges to stored
  ciphertext ranges.

The Builder, Reader, Repacker, Verifier, Consumer, Restoring Consumer, and
Scanner roles are defined by [REMOBJECT]. The Core §14 conformance rule—that
an implementation conforms only for the roles it claims—applies here.

### 2.3. Definitions

- **Canonical plaintext object / inner stream**: the complete byte string
  defined by Core §3.1 and Core §4.
- **Envelope**: the encrypted representation: scalar header, key frame,
  metadata frame, payload frame, footer, and final fill.
- **Data-encryption key (DEK)**: a fresh 32-byte per-object secret at the root
  of the envelope key schedule.
- **Recipient epoch**: one X-Wing key pair identified by a 16-byte
  `recipient_epoch_id` and an optional printable recovery label. Its public
  key is 1216 bytes; its secret custody form is the 32-byte X-Wing seed.
- **Inner `BodyLba`**: a block index within the canonical plaintext object,
  as defined by Core §2.3.
- **Stored `BodyLba`**: a `chunk_size` block index within the stored envelope.
- **Stored bytes**: the complete envelope byte string through the final fill
  byte.

### 2.4. Integer, Byte, and Text Conventions

All fixed-width integers are unsigned and big-endian. Byte offsets are
zero-based. `KiB` = 2^10 bytes and `MiB` = 2^20 bytes. Hexadecimal values use
the `0x` prefix. UTF-8 is [RFC3629]. `roundup(x, C)` is the smallest multiple
of `C` greater than or equal to `x`. SHA-256 is [FIPS180-4]; SHA3-256 and
SHAKE256 are [FIPS202]. All offsets, lengths, counts, and products use checked
unsigned 64-bit arithmetic and MUST NOT wrap silently.

### 2.5. Constants

| Constant | Value | Meaning |
| --- | --- | --- |
| `REM_OBJECT_MAGIC` | `REMO` (`52 45 4D 4F`) | Envelope magic |
| `REM_OBJECT_HEADER_LEN` | 128 | Scalar-header length |
| `REM_OBJECT_FORMAT_VERSION` | 2 | Current envelope `format_version` |
| `REM_OBJECT_SUITE_HKDF_CHACHA` | `0x01` | Current `suite_id` |
| `REM_OBJECT_WRAP_SUITE_XWING` | `0x02` | Current `wrap_suite` |
| `REM_OBJECT_KEY_FRAME_MAGIC` | `REMK` (`52 45 4D 4B`) | Key-frame magic |
| `REM_OBJECT_KEY_FRAME_MIN_LEN` | 1191 | Minimum one-slot key-frame length |
| `REM_OBJECT_KEY_FRAME_MAX_LEN` | 16384 | Maximum key-frame length |
| `REM_OBJECT_KEY_FRAME_MAX_SLOTS` | 8 | Maximum recipient slots |
| `REM_OBJECT_SALT_LEN` | 16 | `hkdf_salt` length |
| `REM_OBJECT_OBJECT_ID_FIELD_LEN` | 64 | Fixed `object_id` field length |
| `REM_OBJECT_TAG_LEN` | 16 | Poly1305 tag length |
| `REM_OBJECT_NONCE_LEN` | 12 | ChaCha20-Poly1305 nonce length |
| `REM_OBJECT_KEY_LEN` | 32 | Derived key length |
| `REM_OBJECT_MAX_METADATA_FRAME_LEN` | 16777216 | Maximum metadata-frame length |
| `REM_OBJECT_MAX_CBOR_NESTING_DEPTH` | 32 | Maximum metadata nesting depth |
| `REM_OBJECT_MAX_METADATA_ITEMS` | 65536 | Maximum metadata item count |
| `REM_OBJECT_FOOTER` | `REMO_STREAM_END.` | 16-byte completion footer |
| `LABEL_SALT` | `rem-encrypt-salt-v1` | Salt-derivation label |
| `LABEL_OBJECT` | `rem-encrypt-object-v1` | Object-secret label |
| `LABEL_METADATA` | `rem-encrypt-metadata-v1` | Metadata-key label |
| `LABEL_PAYLOAD` | `rem-encrypt-payload-v1` | Payload-key label |
| `WRAP_INFO_PREFIX` | `rem-encrypt-wrap-v1` followed by NUL | 20-byte HPKE-info prefix |

## 3. Relationship to REM-OBJECT Core

### 3.1. Wrapped Bytes and Identity

The envelope payload plaintext MUST be the complete canonical byte string
defined by Core §3.1 and Core §4, including the manifest, tar EOF records, and
final zero fill. It MUST NOT be a reconstruction, normalized archive, or
partial stream. The envelope carries the Core `plaintext_digest`, while
`stored_digest` is computed over the complete envelope and remains external
as defined by Core §3.3.

The scalar header's `object_id` field matches Core's intrinsic 1–64-byte
`object_id` bound. After opening, its `object_id` and `chunk_size` MUST equal
the values in the recovered inner stream.

### 3.2. Representation Detection

For a whole-object input, the four leading bytes `REMO` identify the
REM-ENCRYPT representation. Inputs without that magic are not envelopes and
are handled under Core §3.4.

### 3.3. Shared Obligations

Core owns these representation-independent obligations:

- the manifest-anchor obligation and encrypted-copy carve-out (Core §4.7.2);
- the report-all-nonconformities verifier rule (Core §7.4);
- the distinction between I/O failures and format violations, plus the
  no-panic rule (Core §11 and Core §12.9);
- the catalog trust domain (Core §12.6); and
- path, link, attribute, and restore safety (Core §12.10).

This document specifies how the envelope discharges the encrypted-copy
obligations without restating the Core rules.

## 5. Encrypted Representation

### 5.1. Frame Sequence and Version Gate

An encrypted REM-OBJECT has exactly this layout:

```text
scalar header (128) || key frame (K) || metadata frame (M) ||
payload chunks || footer (16) || zero fill
```

The header, key frame, footer, and fill are plaintext. The metadata frame and
payload chunks are ChaCha20-Poly1305 ciphertext. The payload plaintext is the
complete canonical object of Core §4, manifest included. Total stored length
MUST be a positive multiple of `chunk_size`.

`format_version` is an on-tape field, not this document's version. A Reader
MUST accept only value `2` and requires a matching recipient private key. It
MUST NOT attempt another key mode after any parse, key-resolution, unwrap, or
authentication failure. Value `1` is permanently reserved as stated in
Section 10.

### 5.2. Scalar Header

The scalar header is exactly 128 bytes:

| Offset | Length | Name | Type | Required value or meaning |
| --- | ---: | --- | --- | --- |
| `0x00` | 4 | `magic` | ASCII | `REMO` |
| `0x04` | 2 | `header_len` | `uint16` | 128 |
| `0x06` | 1 | `format_version` | `uint8` | 2 |
| `0x07` | 1 | `suite_id` | `uint8` | `0x01` |
| `0x08` | 4 | `chunk_size` | `uint32` | Positive multiple of 512; equal to the inner value |
| `0x0C` | 4 | `flags` | `uint32` | Zero |
| `0x10` | 16 | `reserved` | bytes | Zero |
| `0x20` | 16 | `hkdf_salt` | bytes | Nonzero salt derived by Section 5.5 |
| `0x30` | 8 | `metadata_frame_len` | `uint64` | `M`, including tag; 17 through 16777216 |
| `0x38` | 1 | `wrap_suite` | `uint8` | `0x02` |
| `0x39` | 3 | `reserved` | bytes | Zero |
| `0x3C` | 4 | `key_frame_len` | `uint32` | Canonical key-frame length `K` |
| `0x40` | 64 | `object_id` | UTF-8 field | 1–64 non-NUL bytes, then NUL padding |

Bytes `0x10..0x20` and `0x39..0x3C` MUST be zero. The format, suite, and wrap
values are governed by Section 10. `key_frame_len` MUST be between 1191 and
16384 inclusive. The smallest syntactically possible frame is a one-slot
frame at 1191 bytes; a two-slot frame with empty labels is 2377 bytes.

The `object_id` contains no NUL and is right-padded with zero bytes. Readers
MUST reject an all-zero field, an interior NUL followed by a nonzero byte,
invalid UTF-8, or a value longer than 64 bytes. Its bound and meaning are
owned by Core §4.5.1.

The beginning of an illustrative header with `chunk_size = 4096`,
`metadata_frame_len = 64`, `key_frame_len = 1191`, `object_id = object-2`,
and salt byte `02` repeated 16 times is:

```text
52 45 4d 4f 00 80 02 01 00 00 10 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
02 02 02 02 02 02 02 02 02 02 02 02 02 02 02 02
00 00 00 00 00 00 00 40 02 00 00 00 00 00 04 a7
6f 62 6a 65 63 74 2d 32 00 ... 00
```

The repeated salt illustrates layout only.

### 5.3. The Key Frame and HPKE Wrapping

The key frame begins at byte 128 and occupies exactly `key_frame_len` bytes:

| Relative offset | Length | Field | Constraint |
| --- | ---: | --- | --- |
| `0` | 4 | magic | ASCII `REMK` |
| `4` | 1 | `slot_count` | 1 through 8 |
| repeated | 1 | `slot_index` | Strictly increasing across slots |
| repeated | 16 | `recipient_epoch_id` | Opaque epoch identifier |
| repeated | 1 | `label_len` | 0 through 32 |
| repeated | `label_len` | `epoch_label` | Bytes `0x20` through `0x7E` only |
| repeated | 1120 | `enc` | X-Wing ciphertext: ML-KEM-768 `ct` (1088 bytes) then X25519 `ct_X` (32 bytes) |
| repeated | 48 | `ciphertext` | Wrapped 32-byte DEK plus 16-byte tag |

Its length is:

```text
K = 5 + sum_over_slots(1186 + label_len)
```

A Reader MUST reject truncation, trailing bytes, a non-increasing or duplicate
slot index, a duplicate `recipient_epoch_id`, an invalid label, an invalid
slot count, or a frame outside the header bounds. A Sealer MUST emit at least
one slot, MUST give every slot a distinct `recipient_epoch_id`, and MUST fail
the entire seal if any configured recipient cannot be wrapped.

A Sealer SHOULD ensure single-key-loss survivability by using two or more
independent recipients or by independently protecting the sole recipient
secret. Implementations SHOULD default to at least two recipients and require
explicit opt-in for one. Readers accept any canonical frame with one through
eight slots.

Every slot uses the header's object-global `wrap_suite`; there is no per-slot
discriminator. Suite `0x02` is HPKE Base mode [RFC9180] with X-Wing
[XWING-DRAFT10] as KEM, HKDF-SHA256 as KDF, and ChaCha20-Poly1305 as AEAD.
The HPKE plaintext is the 32-byte DEK; HPKE AAD is empty. Each slot uses fresh
encapsulation randomness. The exact HPKE suite identifier is:

```text
"HPKE" || 0x647a || 0x0001 || 0x0003
```

The exact 103-byte HPKE `info` value is:

```text
"rem-encrypt-wrap-v1\0"                20 bytes
|| object_id_field                      64 bytes
|| recipient_epoch_id                  16 bytes
|| slot_index                            1 byte
|| 0x02                                  1 byte  (format_version)
|| 0x02                                  1 byte  (wrap_suite)
```

This binds the wrapped DEK to the object id, recipient epoch, slot, format
version, and wrap suite. A Reader selects the slot whose epoch id equals the
supplied private key's epoch id; absence is a hard mismatch.

#### 5.3.1. Frozen X-Wing Construction

Suite `0x02` freezes the byte-level construction from
`draft-connolly-cfrg-xwing-kem-10`; later revisions do not silently alter it.

For a 32-byte seed:

```text
expanded = SHAKE256(seed, 96)
d        = expanded[0:32]
z        = expanded[32:64]
sk_X     = expanded[64:96]
```

Run ML-KEM-768 `KeyGen_internal(d, z)` to obtain `(pk_M, sk_M)`, and derive
`pk_X` from `sk_X` using X25519. Serialize:

```text
public key = pk_M || pk_X        # 1184 + 32 = 1216 bytes
secret custody form = seed       # 32 bytes
```

The expanded ML-KEM decapsulation key and `sk_X` are ephemeral and MUST NOT
replace the seed as the secret-at-rest custody unit.

For encapsulation, let `ss_M` and `ct_M` be the ML-KEM-768 outputs, and let
`ss_X` and `ct_X` be the raw X25519 shared secret and ephemeral public key:

```text
enc = ct_M || ct_X               # 1088 + 32 = 1120 bytes

shared_secret =
  SHA3-256(ss_M || ss_X || ct_X || pk_X || 0x5c2e2f2f5e5c)
```

The six-byte suffix is ASCII `\.//^\`. The resulting 32 bytes feed HPKE as
the KEM shared secret. The wrapped-DEK ciphertext is 48 bytes.

The `kem_id` `0x647a` is frozen for suite `0x02`. A final X-Wing RFC that is
wire-identical does not change this suite; Section 10 reserves `0x03` only
for an on-wire divergence. The repository's
`xwing-draft10-kat.txt` and `xwing-wrap-kat.txt` freeze the construction and
HPKE wrapping bytes. The archived draft text at
`specs/publication/provenance/draft-connolly-cfrg-xwing-kem-10.txt` has
SHA-256
`530900ac0519e28eb1ff50bf80ecdb7648add22e500db72b465bab4fb6b6a5ec`.

### 5.4. Key Inputs and Identification

The Sealer generates a fresh uniformly random 32-byte DEK for every seal and
wraps it independently to every recipient. It MUST obtain the DEK and
encapsulation randomness from a fallible operating-system-backed CSPRNG and
MUST fail closed if entropy is unavailable. Recipient public keys and
fingerprints are custody inputs outside the format. A private-key file MUST
contain the canonical 32-byte X-Wing seed, not an expanded decapsulation key.
Any private key named in the frame can decrypt the object by itself.

### 5.5. Salt and Object-Key Derivation

`HKDF(ikm, salt, info, len)` means HKDF-Extract followed by HKDF-Expand
[RFC5869]. Let `metadata_hash` be SHA-256 of the canonical metadata plaintext
and `object_id_field` the exact 64 header bytes. For the first `ctr` in
`0x00..=0xFF` whose output is nonzero:

```text
hkdf_salt = HKDF(DEK, empty,
  "rem-encrypt-salt-v1" || ctr || object_id_field ||
  plaintext_digest || metadata_hash,
  16)
```

The one-byte `ctr` follows the label with no separator. A Sealer MUST derive
the salt and MUST NOT accept it from a caller. A Reader MUST rederive it after
metadata authentication and reject a mismatch.

Define:

```text
header_hash = SHA-256(exact 128-byte scalar header || exact key frame)

object_secret = HKDF(DEK, hkdf_salt,
                     "rem-encrypt-object-v1" || header_hash, 32)
metadata_key  = HKDF(object_secret, empty,
                     "rem-encrypt-metadata-v1", 32)
payload_key   = HKDF(object_secret, empty,
                     "rem-encrypt-payload-v1", 32)
```

Every scalar-header and key-frame byte is therefore bound into the encrypted
frames. In particular, `wrap_suite` is bound here and in the HPKE `info`
transcript. Rewriting a key frame without resealing metadata and payload is
impossible and MUST NOT be attempted.

### 5.6. Metadata Frame

The metadata plaintext is one deterministic-CBOR [RFC8949] map with exactly
four writer entries:

| Key | Value |
| ---: | --- |
| 0 | unsigned integer `1` (`metadata_version`) |
| 1 | positive `plaintext_size`, an exact multiple of `chunk_size` |
| 2 | text `sha256` |
| 3 | 32-byte `plaintext_digest` |

Keys use ascending deterministic order and shortest encodings. A Reader MAY
skip unknown keys but MUST enforce canonical order, unique keys, valid UTF-8,
maximum nesting depth 32, at most 65536 decoded items, required-field types
and values, and no trailing bytes. The accepted CBOR repertoire is unsigned
integers, definite byte/text strings, definite arrays/maps, and simple values
false, true, and null. Negative integers, tags, floats, indefinite forms, and
other simple values are invalid.

The frame is:

```text
ChaCha20-Poly1305(
  metadata_key,
  nonce = 12 zero bytes,
  AAD = empty,
  plaintext = metadata CBOR)
```

It is stored as ciphertext followed by the 16-byte tag. Its stored length
MUST equal `metadata_frame_len`.

### 5.7. Payload Frame

Let `P = plaintext_size` and `C = chunk_size`. Both are authenticated. `P` is
positive and divisible by `C`; therefore `N = P / C` full chunks are emitted.
Chunk `i`, for `0 <= i < N`, uses `payload_key`, empty AAD, and:

```text
nonce = 00 00 00 || uint64_be(i) || final_flag
final_flag = 0x01 exactly when i = N - 1; otherwise 0x00
```

Each stored chunk is exactly `C + 16` bytes: `C` ciphertext bytes followed by
the 16-byte tag. Payload-frame length is `P + 16 * N`. Readers MUST compute
finality and MUST NOT infer it by probing or accept a short final chunk.

### 5.8. Footer, Fill, and Geometry

The completion footer is:

```text
ASCII: REMO_STREAM_END.
hex:   52 45 4d 4f 5f 53 54 52 45 41 4d 5f 45 4e 44 2e
```

It follows the last payload chunk. Zero bytes fill through the next
`C`-byte boundary. Fill is part of the stored bytes and `stored_digest`.
Let `K` be `key_frame_len`:

```text
payload_len       = P + 16 * (P / C)
footer_offset     = 128 + K + M + payload_len
stored_size       = roundup(footer_offset + 16, C)
cipher_offset(i)  = 128 + K + M + i * (C + 16)
```

Given stored size and the public header, a Keyless Verifier computes:

```text
N = floor((stored_size - 128 - K - M - 16) / (C + 16))
footer_offset = 128 + K + M + N * (C + 16)
```

It MUST additionally require `N > 0`, `stored_size mod C = 0`,
`roundup(footer_offset + 16, C) = stored_size`, the footer at the exact
offset, and zero remaining bytes. These checks establish structural
consistency and completion, not authenticity.

### 5.9. Sealing

A Sealer MUST, using checked arithmetic:

1. Validate `C`, `P`, `object_id`, expected digest, and recipient set.
2. Construct the canonical metadata plaintext.
3. Generate the DEK, derive the salt, wrap the DEK to every recipient, and
   serialize the canonical key frame.
4. Serialize the final scalar header, including `M`, salt, wrap suite, and
   `K`.
5. Compute `header_hash` and derive the object, metadata, and payload keys.
6. Emit header, key frame, encrypted metadata, and all full payload chunks
   while recomputing plaintext size and SHA-256.
7. Reject an expected/observed size or digest mismatch and source bytes beyond
   `P`.
8. Emit footer and zero fill.

A failed seal MUST NOT be represented as complete.

### 5.10. Opening, Recovery, and Keyless Inspection

A Keyed Reader MUST parse the scalar header, enforce Section 10, read and
canonically parse the key frame, select the matching epoch, and unwrap the
DEK. It then derives the keys, authenticates and parses metadata, rederives
the salt, decrypts exactly `N` chunks, verifies plaintext size and SHA-256,
verifies footer and fill, and requires end of input. It MUST release no
unauthenticated chunk.

After whole-object authentication, recovery SHOULD validate the inner stream
under Core §7.4 and MUST compare its `REMANENCE.object_id` and
`REMANENCE.chunk_size` with the scalar header before publishing restored
members. Core §12.10 governs path, link, attribute, and extension handling.
Recovery output SHOULD be staged and published only after complete success.

Catalogless recovery is possible from an object and one matching recipient
private key. A Keyless Verifier MAY parse the header and key frame, compute
`stored_digest`, and validate Section 5.8. It MUST describe the result as
public structural consistency, not authenticity or provenance.

## 6. Partial File Restore

The range and inner-block validation rules are owned by Core §6.1 and
Core §6.2. This section maps those validated inner blocks through the
REM-ENCRYPT representation.

### 6.3. Ciphertext Mapping

Inner body block `b` is encrypted chunk `b`. Let `K = key_frame_len` and
`F = 128 + K + metadata_frame_len`:

```text
cipher_offset(b) = F + b * (C + 16)
cipher_len       = C + 16
nonce counter    = b
final_flag       = 0x01 if b == object_chunk_count - 1, else 0x00
```

`object_chunk_count` is the envelope-wide count
`plaintext_size / chunk_size`, not a file's Core `chunk_count`. The Restorer
fetches `[cipher_offset(b), cipher_offset(b) + C + 16)` for every Core-mapped
block, authenticates and decrypts each chunk, concatenates the plaintexts,
and trims under Core §6.2. Consecutive chunks form one contiguous stored
range.

Finality comes from authenticated `plaintext_size`, never probing. A Restorer
MUST NOT release a chunk whose tag fails. It MAY release each authenticated
chunk while streaming, but any later failure aborts the range and MUST be
reported so that a released prefix is not mistaken for a successful complete
range. Encrypted PFR requires a key and one metadata-frame read.

Without a catalog, a Reader opens sequentially through the inner manifest,
then uses its Core index with this mapping.

### 6.4. Stored-Block Mapping

On byte-addressed storage, the Section 6.3 range is fetched directly. On a
fixed-block backend, a stored range `[a, a + l)` maps to:

```text
first_stored_block = floor(a / C)
last_stored_block  = floor((a + l - 1) / C)
```

Because a stored chunk is `C + 16` bytes, one inner block's encrypted form
spans at most three stored blocks. A run of `k` chunks occupies one
contiguous range of at most:

```text
k + ceil(16 * k / C) + 1
```

stored blocks, which is `k + 2` whenever `16 * k <= C`. This tag slip is why
stored and inner `BodyLba` values differ for encrypted copies.

## 7. Envelope Verification Chain

### 7.1. The Chain of Trust

The complete chain is reproduced here so an envelope implementer can see the
encrypted layer and the Core layer together:

```text
off-tape catalog                      on-tape parity-layer bootstrap row
(stored_digest + plaintext_digest     (plaintext copies: manifest location +
 per copy — Core §12.6 trust           manifest_sha256; encrypted copies:
 domain)                               the Core §8.2 envelope fields only)
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

The authenticated `plaintext_digest` covers every canonical byte including
the manifest. It therefore discharges the encrypted-copy manifest-anchor
obligation and carve-out defined by Core §4.7.2. External manifest anchors are
required for plaintext copies only; the manifest entry's own pax digest is an
inner self-consistency check.

### 7.2. Write-Path Verification

Core §7.2 steps 1 and 2 govern payload and canonical-stream verification. At
seal time, the Sealer recomputes the size and digest of the bytes actually
sealed and fails, with no footer, on mismatch. It computes the encrypted
copy's `stored_digest` over the emitted envelope bytes.

### 7.3. Post-Write Re-Verification

Core §7.3 owns the deployment obligation to re-read a copy before recording
it durable. A full encrypted re-verification uses Section 7.4; a minimum
keyless re-read compares `stored_digest` without making an authentication
claim.

### 7.4. Encrypted Verifier Profiles

Core §7.4 requires every Verifier, in every representation, to report all
nonconformities rather than stop at the first reportable error.

- **Keyed encrypted copy**: perform Section 5.10 in full—header and registry
  gates, metadata authentication, salt rederivation, every chunk tag,
  plaintext size and digest, footer, fill, and inner cross-checks—then verify
  the recovered inner stream under Core §7.4 and its identities under
  Core §3.3. Compare `stored_digest` with the catalog value when available.
- **Keyless encrypted copy**: validate Section 5.8 public structure and
  compare `stored_digest`. Report this only as structural consistency.

A successful keyed verification means that every payload byte and all object
structure satisfy the Core verifier and every stored encrypted frame
authenticates under the envelope keys. It does not establish writer identity;
Section 12.7 applies.

## 8. Storage Bindings

### 8.1. Backend Records for Encrypted Copies

Core §8.1 defines the representation-independent backend record. For an
encrypted copy, a backend also records `format_version`,
`metadata_frame_len`, `key_frame_len`, and the recipient epoch ids actually
present. These fields are public envelope geometry and recovery selectors;
they do not replace parsing the authoritative envelope header and key frame.

### 8.2. Tape Bootstrap and Catalogless Recovery

Core §8.2 owns the Writer obligation that an encrypted REM-PARITY bootstrap
row omit plaintext manifest anchors. [REMPARITY] owns the row encoding.

Catalogless recovery begins at stored block 0. The scalar header supplies
`object_id`, `format_version`, and `key_frame_len`; the adjacent key frame
supplies recipient epoch ids. With a matching private key, the Reader opens
sequentially until it reaches the inner manifest. The authenticated
`plaintext_digest` anchors that manifest through Section 7.1. The absence of
a direct manifest location is an accepted recovery-path cost on sequential
media and avoids exposing confidential inner structure.

## 10. Versioning and Registries

Document version, Core stream schema, envelope `format_version`, `suite_id`,
and `wrap_suite` are independent axes and MUST NOT be used as proxies for one
another.

### 10.1. `format_version` Registry

`format_version` is the byte at scalar-header offset `0x06`.

| Value | Status | Meaning |
| ---: | --- | --- |
| `1` | reserved / permanently forbidden | MUST never be accepted or reassigned |
| `2` | **current** | The envelope defined by this document |
| all others | unassigned | Hard error |

An unsupported value produces `UnsupportedFormatVersion`.

### 10.2. `suite_id` Registry

`suite_id` is the AEAD/KDF discriminator at offset `0x07`.

| Value | Status | Meaning |
| ---: | --- | --- |
| `0x01` | **current** | HKDF-SHA-256 + ChaCha20-Poly1305 |
| all others | unassigned | Hard error |

A future AEAD or KDF migration receives a new `suite_id`. An unassigned value
produces `InvalidSuite`.

### 10.3. `wrap_suite` Registry

`wrap_suite` is the KEM discriminator at offset `0x38`.

| Value | Status | KEM |
| ---: | --- | --- |
| `0x00` | reserved / invalid | — |
| `0x01` | **permanently forbidden** | Legacy X25519-only assignment; pre-production and never shipped |
| `0x02` | **current** | X-Wing from `draft-connolly-cfrg-xwing-kem-10` |
| `0x03` | reserved | A future X-Wing construction that differs on the wire from draft-10 |
| all others | unassigned | — |

Per Section 5.3.1, a wire-identical final X-Wing RFC keeps `0x02`; only its
provenance citation changes. Value `0x03` is consumed only if the final
construction changes wire bytes, such as the combiner or `kem_id`.

### 10.4. Assignment and Deprecation Policy

Superseded suites remain valid for **opening**. Sealers MUST use the current
`suite_id` and current `wrap_suite`. Assignment of any new value requires a
published REM-ENCRYPT revision. Reserved, forbidden, and unassigned values
are hard errors—`InvalidWrapSuite`, `InvalidSuite`, or
`UnsupportedFormatVersion`—and MUST NOT be negotiated or guessed.

An envelope change not expressible through a registered discriminator and
ignorable metadata requires a new `format_version` or magic in a published
REM-ENCRYPT revision. No envelope change requires a Core stream-format change
unless it also changes the canonical plaintext object.

## 11. Errors

Core §11 owns the requirement that I/O failures remain distinguishable from
format violations; Core §12.9 owns the requirement that byte-reachable code
never panics, crashes, or allocates unboundedly. Both apply to all envelope
parsing.

### 11.2. Envelope Errors

```text
InvalidMagicBytes            input does not begin with REMO
InvalidHeaderLength          header_len is not 128
UnsupportedFormatVersion     format_version is not 2
InvalidSuite                 suite_id is not 0x01
InvalidChunkSize             chunk_size is zero or not a multiple of 512
ReservedBytesNotZero         flags or reserved bytes are nonzero
InvalidWrapSuite             wrap_suite is not the current or an
                             opening-valid registered suite
InvalidKeyFrameLength        key_frame_len violates bounds
InvalidKeyFrame              malformed or non-canonical REMK frame
RecipientEpochMismatch       no slot matches the supplied private-key epoch
HpkeFailed                   HPKE key parsing, setup, wrap, or unwrap failed
EntropyUnavailable           OS-backed randomness could not be obtained
InvalidSalt                  all-zero hkdf_salt
SaltDerivationMismatch       hkdf_salt differs from Section 5.5
InvalidObjectIdField         object_id field empty, malformed, or invalid UTF-8
MetadataFrameLengthInvalid   metadata_frame_len outside [17, 16 MiB]
UnexpectedEof                declared frame, footer, or fill bytes missing
MissingFinalChunk            payload ends before an authenticated final chunk
AeadAuthenticationFailed     metadata or chunk tag verification failed
InvalidCborEncoding          metadata is not valid metadata-profile CBOR
MissingRequiredMetadataField required metadata key absent
InvalidMetadataField         metadata field has wrong type or value
PlaintextDigestMismatch      recovered bytes disagree with plaintext_digest
PlaintextSizeMismatch        recovered size disagrees with plaintext_size
InvalidFooter                footer bytes wrong at footer_offset
FillNotZero                  nonzero byte in post-footer fill
TrailingData                 bytes follow stored_size in whole-object input
InnerObjectMismatch          inner object_id, chunk_size, or format gates
                             disagree with the envelope
InvalidInput                 sealing input violates Section 5.9
Io                           underlying I/O failure, not a format violation
```

Recommended detection order is Section 5.2 followed by Section 5.10.
Multi-fault inputs may yield any applicable error; vectors are single-fault.
Registry errors follow Section 10, including opening-valid deprecated suites.

## 12. Security Considerations

### 12.1. Per-Object Key Uniqueness

Every seal MUST use a fresh uniformly random 32-byte DEK and fresh HPKE
encapsulation randomness for every recipient, following [RFC9180] §9.2.3.
Entropy failure is fatal. Independent seals of identical Core bytes therefore
use independent keys and normally have different stored bytes.

`header_hash` binds the complete scalar header and key frame. Salt derivation
also binds `object_id`, `plaintext_digest`, and the metadata hash. Reusing an
`object_id` remains forbidden by Core, but independently random DEKs keep
distinct seals independent.

Deterministic vector-generation hooks inject fixed secrets solely for
reproducible conformance artifacts and MUST NOT be exposed as production
sealing modes.

### 12.2. Key Separation and Nonce Safety

The metadata nonce equals the nonce of a non-final payload chunk 0. This is
safe only because `metadata_key` and `payload_key` are distinct. A revision
MUST NOT merge them.

### 12.3. Binding Without AAD

Both encrypted frames use empty AAD. The scalar header and key frame are bound
through `header_hash`; chunk index and finality are bound through the nonce.
Cross-object splicing fails because keys differ. Reordering, duplication, and
truncation fail because the nonce or final-chunk requirement fails.

`wrap_suite` is bound through both `header_hash` and HPKE `info`. Structural
registry rejection occurs before any attempt to interpret a forbidden value.

### 12.4. Fail-Closed

A failed metadata or chunk tag MUST stop processing without releasing that
chunk's plaintext. A failed seal MUST NOT produce a footer. Parity or CRC
failure is repaired before another open attempt. Partial output is never
reported as a successful complete open or range read.

### 12.5. Confidentiality Boundary, Public Facts, and Catalog Trust

An encrypted copy reveals: its REM-ENCRYPT identity; the registered format
and suites; `chunk_size`; recipient epoch ids and labels; salt; public frame
lengths; `object_id`; and stored length. Exact `plaintext_size` and chunk
count are derivable from Section 5.8. It reveals no member names, member
sizes, member count, manifest content, or payload bytes.

The off-tape catalog contains cleartext paths, per-file rows, and digests.
Core §12.6 owns this catalog trust domain and its external-anchor obligation.
The on-tape REM-PARITY bootstrap for an encrypted copy is deliberately
minimal: it carries only public fields and, per Core §8.2, omits manifest
anchors. This is the single confidentiality rationale for that structural
rule.

Deployments that treat object existence, identifier, or approximate size as
sensitive must add policy above this format. REM-ENCRYPT defines no payload
padding.

### 12.7. Non-Committing AEAD

ChaCha20-Poly1305 is not key-committing [AEAD-COMMIT] [PART-ORACLE].
`stored_digest` does not prevent equivocation because one byte string can be
constructed to open under multiple candidate keys. REM-ENCRYPT claims
confidentiality and self-consistency, not writer identity or provenance.
Possession of recipient public keys permits fabrication of a new internally
valid object. Deployments needing provenance require an independently
authenticated or signed external manifest.

### 12.8. Key Rotation and Epoch Longevity

Recipient rotation affects new seals. Because the key frame contributes to
`header_hash`, rewrapping without resealing is forbidden. A private epoch key
MUST NOT be destroyed while any live object references it.

Resealing opens an envelope and seals the identical canonical bytes to a new
recipient set. It preserves the Core `object_id`, `chunk_size`, canonical
bytes, and `plaintext_digest`; it uses a fresh DEK and changes envelope bytes
and `stored_digest`.

### 12.9. Envelope Hostile-Input Discharge

Core §11 and Core §12.9 apply to every envelope parser. In addition, envelope
parsers MUST enforce:

- the fixed 128-byte scalar header before interpreting variable data;
- `metadata_frame_len` in `[17, 16 MiB]`;
- the Section 5.6 metadata CBOR depth and item limits incrementally;
- key-frame length and slot-count bounds before allocation;
- checked geometry before seeking or allocating; and
- O(1) memory per payload chunk.

The envelope fuzz-target list is: scalar-header parser, key-frame parser,
metadata CBOR decoder, and whole-object open/verify for encrypted inputs. The
Core §12.9 fuzz targets remain separate.

### 12.11. Threat Model and Secret Handling

| Attacker capability | Can read or do | Cannot read, assuming sound primitives and custody |
| --- | --- | --- |
| Steals only object media | Public header/key-frame facts and exact size | Metadata and payload |
| Compromises the sealing host | Host plaintext, in-memory keys, contemporaneous and future seals while compromise persists | Earlier objects whose DEKs and recipient secrets are absent |
| Holds one recipient private key | Every object wrapped to that epoch | Objects not wrapped to that epoch |
| Holds only recipient public keys | Create new internally valid objects; observe public facts | Existing-object plaintext |

Recipient public keys MUST be pinned independently if substitution is in
scope. Multiple recipients reduce loss risk but are not a cryptographic
threshold: any one matching private key opens the object.

Implementations SHOULD minimize copies of DEKs, derived keys, private keys,
HPKE ephemeral secrets, and RNG state, and MUST promptly zeroize mutable
secret buffers. Secrets MUST NOT appear in logs, diagnostics, command lines,
core dumps, or durable plaintext staging. Whole-object recovery SHOULD stage
plaintext on protected storage and publish it only after full verification.

The X-Wing hybrid addresses harvest-now-decrypt-later exposure while retaining
X25519 as a classical hedge. X-Wing's malicious-key and malicious-ciphertext
binding properties matter because one DEK is independently wrapped to
multiple recipient keys. The static draft-10 KATs, independent verifier, and
constant-time integration review are reference-release controls. A verified
ML-KEM implementation does not by itself verify the HPKE transcript, combiner
glue, serialization, entropy handling, or zeroization.

Resealing under a future suite requires reading, opening, and writing the
whole object, and on append-only media produces a new object copy. Deployments
SHOULD combine resealing with planned media migration.

## 13. Envelope Test Vectors

The authoritative companion archive is `remanence-test-vectors.tar`,
SHA-256
`77be73e780e9ff2c265c8357b6ba684b4c69800213820ae1331850f742b1d83d`.
Its `MANIFEST.tsv` inventories every vector manifest and artifact,
`CHECKSUMS.sha256` authenticates them, and `verify.py` checks the archive
without a source checkout. Exact byte strings and digests in the archive,
not abbreviated prose, are authoritative.

Core §13 owns plaintext vectors. This section owns encrypted positive objects,
component KATs, encrypted range vectors, and envelope negative vectors. Core
§14 owns the general conformance roles; a claimed REM-ENCRYPT role MUST pass
the applicable vectors here.

### 13.1. REM-OBJECT-TV-E2

`rem-object/objects/rem-object-tv-e2.rem-object` seals the exact canonical
bytes of Core vector `REM-OBJECT-TV-P1`.

| Input | Value |
| --- | --- |
| DEK | byte `7d` repeated 32 times |
| HPKE RNG seed | byte `c3` repeated 32 times |
| Slot 0 | index 0; epoch id `61` repeated 16 times; label `archive-2026-01`; X-Wing seed `51` repeated |
| Slot 1 | index 1; epoch id `62` repeated 16 times; label `recovery-2026-01`; X-Wing seed `52` repeated |

Expected geometry:

| Quantity | Value |
| --- | --- |
| `plaintext_size` / chunks | 20480 / 5 |
| `key_frame_len` | 2408 |
| Metadata plaintext / frame | 50 / 66 bytes |
| Payload frame | bytes 2602–23161 |
| `footer_offset` | 23162 |
| Stored size / blocks | 24576 / 6 |
| `plaintext_digest` | `d59a4a3e4cf2c447c8ed402b109fbb4060ca84dc5b1cebbdc3acb8ca62d8888c` |
| `stored_digest` | `a9c997b5ba66e4d7297594ca68334d736c75c0e728f8cc47b529b9d5eea63e2c` |

The vector manifest pins both recipient public keys, encapsulations,
wrapped-DEK ciphertexts, HPKE intermediate values, salt, all derived keys,
header, key frame, metadata, payload digest, footer, fill, and both object
digests. `plaintext_digest` MUST equal Core vector P1's `stored_digest`.

The deterministic generation hook exists only for artifact reproduction.
Production uses Section 5.4 randomness. The independent verifier implements
OPEN from this document using generic primitives and verifies both slots, the
canonical bytes, manifest, and per-file digests.

### 13.2. REM-OBJECT-TV-D1 Encrypted Copy

The default-chunk vector uses `chunk_size = 262144` and seals the canonical
Core D1 object.

| Quantity | Value |
| --- | --- |
| Plaintext size / chunks | 1048576 / 4 |
| `key_frame_len` | 2402 |
| Metadata plaintext / frame | 52 / 68 bytes |
| Payload frame | bytes 2598–1051237 |
| `footer_offset` | 1051238 |
| Stored size / blocks | 1310720 / 5 |
| `plaintext_digest` | `5d07a7aca146a80dfae22f06de976924a6f5c95aceff119b057894a3ab8e1bf5` |
| `stored_digest` | `69fa820b0c1ab581ac5e04383865b3ac9d15952e0b1e144721377836a696f859` |

The additive `encrypted-last-object-chunk` range vector uses
REM-OBJECT-TV-D1's manifest range: `first_inner_chunk = 3`,
`range_start = 0`, and `range_len = 351`. Since D1 has
`object_chunk_count = 4`, this range covers the true final object chunk
(`i = object_chunk_count - 1`) and MUST authenticate with `final_flag = 1`,
reproducing the exact manifest CBOR, `manifest_sha256`, and
`plaintext_digest`. The paired
`encrypted-last-object-chunk-wrong-finality` negative applies
`final_flag = 0` to that same ciphertext and MUST produce
`AeadAuthenticationFailed`.

### 13.3. X-Wing and HPKE Component KATs

`rem-object/kats/xwing-draft10-kat.txt` pins seed-to-key, encapsulation, and
shared-secret values. `rem-object/kats/xwing-wrap-kat.txt` pins the exact
Section 5.3 HPKE transcript and wrapped DEK for `object_id = object-a`, epoch
id byte `03` repeated 16 times, slot 0, DEK byte `09` repeated 32 times, seed
byte `07` repeated 32 times, and deterministic encapsulation randomness byte
`42` repeated 64 times.

### 13.4. Negative Vectors

Each vector contains exactly one fault and asserts the mapped Section 11.2
error.

**Header.** Wrong magic; `header_len != 128`; unsupported `format_version`
(including permanently forbidden value 1); unknown `suite_id`; `chunk_size`
zero or not a multiple of 512; nonzero `flags`; nonzero bytes in either
reserved region; unknown or reserved `wrap_suite`; forbidden
`wrap_suite = 0x01`; `wrap_suite = 0x00` with a nonempty frame; suite `0x02`
with zero, undersized, or oversized `key_frame_len`; all-zero `hkdf_salt`;
all-NUL, interior-NUL, or non-UTF-8 `object_id`; and
`metadata_frame_len = 16` or greater than 16 MiB.

**Cryptographic binding.** A flipped salt bit; a structurally valid key-frame
label, encapsulation, wrapped-DEK ciphertext, slot insertion, or slot removal
tamper; a flipped ciphertext bit in chunk 1; chunks 1 and 2 transposed; wrong
finality (a sixth chunk appended or the final chunk re-sealed non-final);
authenticated metadata deliberately misstating `plaintext_digest`; and an
object sealed under an arbitrary non-derived header salt. These MUST map to
the applicable `AeadAuthenticationFailed`, `HpkeFailed`,
`PlaintextDigestMismatch`, or `SaltDerivationMismatch` error recorded by the
vector.

**Metadata.** Each metadata-profile repertoire violation (float, tag,
indefinite length, duplicate key, non-shortest encoding); missing key 1;
`metadata_version = 2`; `plaintext_size` zero, not a multiple of
`chunk_size`, or large enough to overflow geometry.

**Framing.** EOF inside the metadata frame; EOF mid-chunk; payload absent
after metadata (`MissingFinalChunk`); footer bytes wrong at the correct
offset; one nonzero fill byte (`FillNotZero`); and bytes appended past the
fill (`TrailingData` from keyed open/verify—the keyless classification of
the same input is advisory under Section 5.8).

**Inner cross-checks.** The defective-Sealer vectors cover an inner
`object_id` differing from the header, inner `chunk_size` differing from the
header, and inner `REMANENCE.encryption` other than `none`; each produces
`InnerObjectMismatch`.

**Writer inputs.** Sealing input whose size is not a multiple of
`chunk_size`; an `object_id` longer than 64 bytes; recipient counts zero, one
without the explicit single-recipient opt-in, or greater than eight;
duplicate epoch ids; non-canonical slot order; and entropy failure.

**Key-frame structure and key use.** Slot counts 0 and 9, duplicate or
misordered slot indices, duplicate `recipient_epoch_id` values, internal slot
truncation, trailing frame bytes, malformed `REMK` magic, malformed
encapsulation, and a wrong recipient private key. A positive case opens a
structurally valid one-slot object: a Sealer MAY emit one through eight slots
subject to the Section 5.3 survivability guidance, and Readers accept one
through eight.

## 15. Identifier Allocation Considerations

The `REMO` and `REMK` magics, `format_version`, `suite_id`, `wrap_suite`,
derivation labels, and HPKE-info prefix are assigned by this document and
governed exclusively by Section 10. The HPKE `kem_id = 0x647a` is frozen as
part of suite `0x02`; this document makes no IANA request.

## 16. References

### 16.1. Normative References

- [REMOBJECT] — "REM-OBJECT Core Format 1.0", companion specification.
- [REMPARITY] — "Rem Tape Parity (REM-PARITY) Format, Version 1.0",
  companion specification; normative for the tape binding.
- [RFC2119] — Bradner, S., "Key words for use in RFCs to Indicate
  Requirement Levels", BCP 14, RFC 2119, March 1997,
  <https://www.rfc-editor.org/info/rfc2119>.
- [RFC8174] — Leiba, B., "Ambiguity of Uppercase vs Lowercase in RFC 2119
  Key Words", BCP 14, RFC 8174, May 2017,
  <https://www.rfc-editor.org/info/rfc8174>.
- [RFC3629] — Yergeau, F., "UTF-8, a transformation format of ISO 10646",
  STD 63, RFC 3629, November 2003,
  <https://www.rfc-editor.org/info/rfc3629>.
- [RFC5869] — Krawczyk, H. and P. Eronen, "HMAC-based Extract-and-Expand
  Key Derivation Function (HKDF)", RFC 5869, May 2010,
  <https://www.rfc-editor.org/info/rfc5869>.
- [RFC8439] — Nir, Y. and A. Langley, "ChaCha20 and Poly1305 for IETF
  Protocols", RFC 8439, June 2018,
  <https://www.rfc-editor.org/info/rfc8439>.
- [RFC7748] — Langley, A., Hamburg, M., and S. Turner, "Elliptic Curves for
  Security", RFC 7748, January 2016,
  <https://www.rfc-editor.org/info/rfc7748>.
- [RFC8949] — Bormann, C. and P. Hoffman, "Concise Binary Object
  Representation (CBOR)", STD 94, RFC 8949, December 2020,
  <https://www.rfc-editor.org/info/rfc8949>.
- [RFC9180] — Barnes, R., Bhargavan, K., Lipp, B., and C. Wood, "Hybrid
  Public Key Encryption", RFC 9180, February 2022,
  <https://www.rfc-editor.org/info/rfc9180>.
- [FIPS180-4] — National Institute of Standards and Technology, "Secure
  Hash Standard (SHS)", FIPS PUB 180-4, August 2015,
  <https://doi.org/10.6028/NIST.FIPS.180-4>.
- [FIPS202] — National Institute of Standards and Technology, "SHA-3
  Standard", FIPS PUB 202, August 2015,
  <https://doi.org/10.6028/NIST.FIPS.202>.
- [FIPS203] — National Institute of Standards and Technology,
  "Module-Lattice-Based Key-Encapsulation Mechanism Standard", FIPS PUB 203,
  August 2024, <https://doi.org/10.6028/NIST.FIPS.203>.

### 16.2. Informative References

- [XWING-DRAFT10] — Connolly, D., Schwabe, P., and B. E. Westerbaan,
  "X-Wing: general-purpose hybrid post-quantum KEM",
  `draft-connolly-cfrg-xwing-kem-10`, 2 March 2026,
  <https://datatracker.ietf.org/doc/html/draft-connolly-cfrg-xwing-kem-10>.
- [AEAD-COMMIT] — Albertini, A., et al., "How to Abuse and Fix
  Authenticated Encryption Without Key Commitment", USENIX Security 2022,
  <https://www.usenix.org/conference/usenixsecurity22/presentation/albertini>.
- [PART-ORACLE] — Len, J., Grubbs, P., and T. Ristenpart, "Partitioning
  Oracle Attacks", USENIX Security 2021,
  <https://www.usenix.org/conference/usenixsecurity21/presentation/len>.
- [LIBCRUX-MLKEM] — Cryspen, "Verifying Libcrux's ML-KEM", 30 January 2024,
  <https://cryspen.com/post/ml-kem-verification/>.
- [REMANENCE] — "Remanence", reference implementation,
  <https://github.com/archivetechie/remanence>.

---

## Appendix A. Worked Envelope Example (Informative)

For REM-OBJECT-TV-E2, `P = 20480` and `C = 4096`, so there are five chunks.
The metadata plaintext is 50 bytes and `M = 66`. The two-slot key frame has
`K = 2408`. The payload begins at `128 + 2408 + 66 = 2602` and occupies
`20480 + 5 * 16 = 20560` bytes. Therefore `footer_offset = 23162`; footer and
1398 fill bytes produce `stored_size = 24576`, or six blocks.

The metadata zero nonce equals non-final payload chunk 0's nonce. Section 12.2
explains why distinct keys make this safe.

## Appendix B. Design Rationale (Informative)

### B.1. Encrypted Chunk Size Equals the Core Block

Using `C` plaintext bytes per encrypted chunk preserves a one-to-one inner
block/chunk identity and keeps range mapping closed-form. The accepted cost is
16 tag bytes per chunk and the stored-block slip quantified in Section 6.4.

### B.2. Full Final Chunks Only

Core guarantees a positive length divisible by `C`, so the final encrypted
chunk is full. This removes short-final and empty-payload cases from range and
verification logic.

### B.3. The Manifest Remains Confidential

Sealing the whole canonical stream hides filenames, sizes, count, and
structure. A clear manifest beside encrypted payloads would defeat the
confidentiality goal.

### B.4. Stored Fill Is Part of the Envelope

Footer-to-block-boundary fill belongs to stored bytes, parity, and
`stored_digest`. This yields one backend-independent byte string and exact
keyless footer geometry.

### B.5. Two Metadata Layers Remain Separate

The Core manifest is the per-file index. Envelope metadata carries only facts
needed to open the encrypted representation. Merging them would expose or
duplicate the index and weaken hostile-input bounds.

### B.6. Empty AAD

Header and key-frame binding live in the key schedule; chunk position and
finality live in the nonce. Repeating the same bindings as AAD would add a
second mechanism.

### B.7. Derived Salts

The salt is derived from the random DEK and authenticated object facts rather
than accepted as a separate randomness input. It remains reproducible after
DEK recovery and is verified on open.

### B.8. Fixed Metadata Is Bound into the Salt

The metadata plaintext is a deterministic function of the Core object, and
its hash contributes to salt derivation. This prevents a future optional
metadata field from creating a readable nonce-reuse state under the same
metadata key.

### B.9. Encrypted Bootstrap Rows Omit Manifest Anchors

Manifest geometry correlates with member count and is confidential. The
authenticated whole-object digest already anchors the decrypted manifest.
Catalogless recovery trades direct manifest positioning for sequential open.

## Author's Address

The ArchiveTech Project
Website: https://archivetech.org
Email: specs@archivetech.org
Reference implementation: https://github.com/archivetechie/remanence
