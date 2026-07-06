# rao-metadata formal specification

Target: `verif/rao-metadata/src/lib.rs`, a proof-facing extraction of the RAO
AEAD metadata writer schema and validation arithmetic in
`crates/remanence-aead/src/metadata.rs`.

The production codec writes deterministic CBOR bytes and decodes a richer
profile that can skip unknown extension keys. This extraction models the exact
required writer-schema core: the four required keys, version `1`, digest
algorithm `sha256`, 32-byte digest payload, no trailing data, and the
plaintext/chunk-size validation arithmetic. The SHA-256 digest payload is
represented as four opaque `u64` words to preserve identity without proving
byte-slice copying.

## M1 — validation success (`validate_metadata_core_success`)

For any metadata core where:

- `chunk_size > 0`
- `chunk_size` is a multiple of 512
- `plaintext_size > 0`
- `plaintext_size` is an exact multiple of `chunk_size`
- `16 * (plaintext_size / chunk_size)` does not overflow `u64`
- `plaintext_size + tag_bytes` does not overflow `u64`

`validate_metadata_core` succeeds.

## M2 — fail-closed rejection (`validate_metadata_core_rejects_*`,
`checked_*_rejects_overflow`)

The validator rejects zero chunk size, non-512-multiple chunk size, zero
plaintext size, and non-aligned plaintext size. The checked arithmetic helpers
reject payload tag multiplication overflow and payload-size addition overflow
with `InvalidMetadataField`, which is the error propagated by the validator.

## M3 — deterministic writer shape (`encode_metadata_core_emits_writer_schema`)

For every valid metadata core, `encode_metadata_core` emits the required v1
writer schema:

- map length = 4
- key order = `0, 1, 2, 3`
- metadata version = 1
- digest algorithm = `sha256`
- digest byte length = 32
- no trailing data

and carries through `plaintext_size` and the digest words.

## M4 — decode/encode round trip (`decode_encode_metadata_core_round_trip`)

For every valid metadata core:

```text
decode_metadata_core(encode_metadata_core(metadata)) = metadata
```

This is a writer-schema core theorem, not a full theorem over production
`Vec<u8>`, UTF-8, byte-slice copying, extension-key skipping, or recursive CBOR
decoding.

## M5 — writer-shape rejection (`decode_metadata_core_rejects_*`)

The decoder rejects trailing data, missing/wrong required map shape, wrong
version, wrong digest algorithm, wrong digest length, and invalid metadata
values after decode.

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties the proof-facing extraction back to
selected production snippets in `crates/remanence-aead/src/metadata.rs`; if it
fires, the extraction and Lean proofs must be re-established.
