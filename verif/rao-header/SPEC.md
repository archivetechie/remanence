# rao-header formal specification

Target: `verif/rao-header/src/lib.rs`, a proof-facing extraction of the scalar
RAO AEAD header layout in `crates/remanence-aead/src/header.rs` and canonical
key-frame rules in `crates/remanence-aead/src/key_frame.rs`.

The production header is a 128-byte byte array with a UTF-8 object id field and
two 16-byte binary fields. This extraction models the fixed scalar checks and
field movement, plus boolean validity facts for key id, salt, and object id. It
does not model the exact object id string, exact 16-byte array contents,
SHA-256 header hashing, allocation, or UTF-8 reconstruction.

## H1 — validation success (`validate_header_core_success`)

For any header core where:

- `chunk_size > 0`
- `chunk_size` is a multiple of 512
- key id is nonzero
- HKDF salt is nonzero
- `17 <= metadata_frame_len <= 16 MiB`
- object id field is valid

`validate_header_core` succeeds.

## H2 — validation rejection (`validate_header_core_rejects_*`)

The validator rejects:

- zero or non-512-multiple chunk sizes
- zero key id
- zero HKDF salt
- metadata-frame lengths below 17 or above 16 MiB
- invalid object id fields

These match the production `RaoHeader::validate` branch order at the scalar
level.

## H3 — frozen field emission (`serialize_header_core_emits_frozen_fields`)

For every valid header core, `serialize_header_core` emits canonical RAO header
wire fields:

- magic = `RAO1`
- header length = 128
- format version = 1
- suite id = 1
- flags = 0
- reserved bytes at `0x38..0x40` = zero

and carries through `chunk_size`, metadata-frame length, and the key/salt/object
validity facts.

## H4 — parse/serialize round trip (`parse_serialize_header_core_round_trip`)

For every valid header core:

```text
parse_header_core(serialize_header_core(header)) = header
```

This is the first incremental RAO format-correctness theorem. It is a header
core theorem, not a full `RaoHeader::parse(RaoHeader::serialize(x)) = x` theorem
over all 128 bytes and the object id string.

## H5 — frozen field rejection (`parse_header_core_rejects_*`)

The parser rejects mismatched frozen fields before accepting the header core:
bad magic, bad header length, unsupported format version, invalid suite id,
nonzero flags, and nonzero reserved bytes.

## H6 — v2 and key-frame status (not formally proved)

Production unit tests, negative vectors, fuzz targets, and the Rust drift guard
cover v1/v2 disjoint parsing, the v2 scalar layout, and canonical key-frame
bytes. The Aeneas-generated `Funs.lean` file does not contain v2 parser or
key-frame functions, so none of those v2 properties are claimed as Lean
theorems. Follow-up **RAO-V2-FORMAL-HEADER-KEY-FRAME** must extract the actual
byte parser/serializer and key-frame codec before round-trip or disjointness
coverage can be advertised as formal.

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties the proof-facing extraction back to
selected production snippets in `crates/remanence-aead/src/header.rs`; if it
fires, the extraction and Lean proofs must be re-established.
