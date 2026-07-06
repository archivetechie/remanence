# rao-archive formal specification

Target: `verif/rao-archive/src/lib.rs`, a proof-facing extraction of the RAO
archive composition core.

This crate models the scalar checks that tie the already-proved RAO header,
metadata, and fixed-capacity manifest-array proof surfaces together:

- the header carries one nonzero object id, one valid chunk size, nonzero
  key/salt facts, and a bounded metadata-frame length
- the metadata carries plaintext size and SHA-256 digest words, with
  plaintext-size alignment and AEAD tag overflow checks under the header chunk
  size
- the manifest-array carries the same object id and chunk size, a bounded
  five-entry shape, nonzero ids, distinct path/file ids, and hardlink target
  accumulation across the two modeled regular entries
- the archive validator requires header/manifest object id equality and
  header/manifest chunk-size equality

The extraction is deliberately scalar. It does not prove exact CBOR bytes,
arbitrary `Vec`/`String` traversal, path syntax, tar/pax records, hashing,
encryption, allocation, IO, or production `BTreeSet`/`BTreeMap` internals. The
component details remain covered by:

- `verif/rao-header/SPEC.md`
- `verif/rao-metadata/SPEC.md`
- `verif/rao-manifest/SPEC.md`

## A1 — archive encode/decode round trip (`decode_encode_archive_core_round_trip`)

For every archive accepted by `validate_archive_core`:

```text
decode_archive_core(encode_archive_core(archive)) = archive
```

More precisely, the theorem proves that there exists a deterministic wire value
emitted by `encode_archive_core` and accepted by `decode_archive_core`:

```text
∃ wire,
  encode_archive_core archive = Ok wire ∧
  decode_archive_core wire = Ok archive
```

The proof covers these top-level wire-shape facts:

- header magic, length, format version, suite id, zero flags, and reserved-zero
  field
- metadata map length, version, SHA-256 digest algorithm flag, digest byte
  length, and absence of trailing data
- manifest root map length, five-entry array length, schema version, empty
  object metadata, empty external references, and absence of trailing data
- field preservation for object id, caller object id, chunk size, metadata
  plaintext size, digest words, fixed manifest entry ids, nonempty regular
  file size, hardlink target id, and symlink target id

The decoder reconstructs an `ArchiveCore` from the wire shape and re-runs
`validate_archive_core`, so the theorem also covers the archive-level
consistency checks on the reconstructed value.

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties this archive extraction back to the
proof-facing RAO header, metadata, and manifest-array crates; if it fires, this
composition proof must be re-established.
