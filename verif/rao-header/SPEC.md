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

## H6 — v1/v2 disjoint dispatch and v2 scalar round trip

`v1_v2_dispatch_disjoint` proves that no format version can enter both parser
branches. For a valid v2 envelope scalar header, including zero `key_id`, HPKE
wrap suite 1, reserved-zero bytes, and `103 <= key_frame_len <= 4096`,
`parse_serialize_v2_header_round_trip` proves the scalar round trip.

## H7 — canonical key-frame round trip

For a nonempty frame of at most eight slots with strictly increasing slot
indexes and printable labels of at most 32 bytes,
`parse_serialize_key_frame_round_trip` proves the abstract grammar round trip.
Rust byte-exact vectors and the drift guard bind this abstract result to the
`RAOK` framing and fixed-width epoch/enc/ciphertext fields.

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties the proof-facing extraction back to
selected production snippets in `crates/remanence-aead/src/header.rs`; if it
fires, the extraction and Lean proofs must be re-established.
