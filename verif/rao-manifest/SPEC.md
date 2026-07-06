# rao-manifest formal specification

Target: `verif/rao-manifest/src/lib.rs`, a proof-facing extraction of the RAO
manifest regular-file writer schema and validation arithmetic in
`crates/remanence-format/src/layout.rs` and
`crates/remanence-format/src/manifest.rs`.

The production codec writes deterministic CBOR bytes, validates the manifest
profile, validates global-pax anchors, supports arbitrary file arrays,
nonregular entries, xattrs, and full text/byte values. This extraction models a
small but central writer-schema core: one regular payload file, the required
seven-key root map in canonical writer order, the required regular-file fields,
empty `object_metadata`, empty `external_references`, empty
`metadata_preservation_data`, `schema_version = 1`, `file_sha256` length 32,
and the chunk-count arithmetic used by both writer and reader.

Text values are represented by opaque scalar ids and SHA-256 bytes by four
opaque `u64` words. This preserves identity for the proof without proving
`String`, `Vec`, exact CBOR bytes, UTF-8 decoding, tar/pax layout, hashing,
xattrs, nonregular entries, global-pax cross-checking, or arbitrary-length
manifest arrays.

## M1 — chunk-count arithmetic (`chunk_count_core_success`)

For any valid chunk size and file size, `chunk_count_core` returns:

- `0` when `size_bytes = 0`
- `(size_bytes - 1) / chunk_size + 1` when `size_bytes > 0`

The proof includes fail-closed rejection of invalid chunk sizes and checked
addition overflow.

## M2 — regular-file validation (`validate_regular_file_core_success`)

For every regular-file core with nonzero path/file ids, executable tag in the
writer set (`null`, `false`, `true`), valid chunk size, and matching
`first_chunk_lba` nullability:

- zero-size files have no first-chunk LBA
- nonzero-size files have a first-chunk LBA

`validate_regular_file_core` succeeds.

## M3 — deterministic writer shape (`encode_manifest_core_emits_writer_schema`)

For every valid one-regular-file manifest core, `encode_manifest_core` emits:

- root map length = 7
- root key order = `object_id`, `chunk_size`, `file_entries`,
  `schema_version`, `object_metadata`, `caller_object_id`,
  `external_references`
- file-entry array length = 1
- `schema_version = 1`
- empty object metadata and external references
- regular-file map length = 8
- regular-file key order = `path`, `file_id`, `executable`, `size_bytes`,
  `chunk_count`, `file_sha256`, `first_chunk_lba`,
  `metadata_preservation_data`
- `file_sha256` byte length = 32
- empty `metadata_preservation_data`
- no trailing data

and carries through object/caller ids, file ids, file size, digest words,
executable tag, and first-chunk LBA.

## M4 — decode/encode round trip (`decode_encode_manifest_core_round_trip`)

For every valid one-regular-file manifest core:

```text
decode_manifest_core(encode_manifest_core(manifest), manifest.chunk_size)
  = manifest
```

This is a writer-schema core theorem, not a full theorem over production CBOR
bytes, `Vec`, UTF-8, tar/pax records, hashing, nonregular entries, xattrs, or
arbitrary-length file arrays.

## M5 — writer-shape rejection (`decode_manifest_core_rejects_*`,
`decode_regular_file_core_rejects_*`)

The decoder rejects trailing data, missing/wrong root map shape, wrong schema
version, reader chunk-size mismatch, wrong file-entry count, non-empty
object/external metadata containers, missing/wrong regular-file map shape, wrong
`file_sha256` byte length, wrong `chunk_count`, and invalid
`first_chunk_lba` nullability.

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties the proof-facing extraction back to
selected production snippets in `crates/remanence-format/src/layout.rs` and
`crates/remanence-format/src/manifest.rs`; if it fires, the extraction and Lean
proofs must be re-established.
