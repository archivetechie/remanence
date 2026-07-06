# rao-manifest formal specification

Target: `verif/rao-manifest/src/lib.rs`, a proof-facing extraction of RAO
manifest writer-schema and validation arithmetic in
`crates/remanence-format/src/layout.rs` and
`crates/remanence-format/src/manifest.rs`.

The production codec writes deterministic CBOR bytes, validates the manifest
profile, validates global-pax anchors, supports arbitrary file arrays,
nonregular entries, xattrs, and full text/byte values. This extraction models a
small but central writer-schema cores:

- the original one-regular-payload-file manifest, with the required seven-key
  root map, required regular-file fields, empty `object_metadata`, empty
  `external_references`, empty `metadata_preservation_data`,
  `schema_version = 1`, `file_sha256` length 32, and chunk-count arithmetic
- a bounded five-entry manifest containing a nonempty regular file with one
  xattr, an empty regular file, a hardlink, a symlink, and a directory
- a fixed-capacity array/fold validator over that five-entry shape, adding
  duplicate path/file-id rejection and hardlink target accumulation across the
  preceding regular entries
- a planner-fold bridge step for arbitrary entry sequences, abstracting the
  production `BTreeSet` lookups as scalar membership facts and proving that an
  arbitrary Lean list of valid generated fold steps reaches a final state
- a membership-facts bridge that models the production `BTreeSet::insert`
  contract as scalar contains/insert facts before feeding the planner fold

Text values are represented by opaque scalar ids and SHA-256 bytes by four
opaque `u64` words. Xattr names and values are represented by scalar ids and a
value length. This preserves identity for the proof without proving `String`,
`Vec`, exact CBOR bytes, UTF-8 decoding, tar/pax layout, hashing, arbitrary
xattr maps, global-pax cross-checking, standard-library `BTreeSet` internals,
or the real Rust `Vec` loop.

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

## M6 — bounded multi-entry manifest round trip (`decode_encode_manifest_entries_core_round_trip`)

For every valid bounded five-entry manifest core:

```text
decode_manifest_entries_core(
  encode_manifest_entries_core(manifest),
  manifest.chunk_size
) = manifest
```

The bounded core covers these production-sensitive entry branches:

- a nonempty regular file with one modeled xattr
- an empty regular file with null first-chunk LBA
- a hardlink whose target path id must be the preceding regular file path id
- a symlink with a nonempty modeled target id
- a directory entry with no file hash and no link target

The proof also includes validation success for the fixed entry forms and
fail-closed rejection of a bad hardlink target after the local entry checks
pass. This is still a scalar writer-schema theorem: it does not prove arbitrary
manifest array traversal, exact CBOR bytes, duplicate detection, production
`BTreeMap` xattr ordering, path syntax validation, or full production
`validate_manifest(encode_manifest(x))` over `Vec`/`String` values.

## M7 — fixed-capacity array/fold round trip (`decode_encode_manifest_array_core_round_trip`)

For every valid fixed five-entry manifest array core:

```text
decode_manifest_array_core(
  encode_manifest_array_core(manifest),
  manifest.chunk_size
) = manifest
```

This strengthens M6 with array-level state:

- all five modeled payload path ids are pairwise distinct
- all five modeled file ids are pairwise distinct
- the hardlink target must be one of the two regular path ids accumulated
  before the hardlink entry

The proof includes helper theorems for `distinct5_core` success, duplicate-pair
rejection, hardlink-target success when the target is in the regular prefix,
and fail-closed rejection when the target is unseen. This models the production
planner's duplicate sets and the manifest reader's `seen_regular_paths` fold at
fixed capacity. It still does not prove arbitrary `Vec` traversal,
production `BTreeSet` internals, exact CBOR bytes, path syntax validation,
or full production `validate_manifest(encode_manifest(x))` over `Vec`/`String`
values.

## M8 — planner fold bridge (`planner_fold_core_success_arbitrary`)

The production planner walks arbitrary payload entries while maintaining:

- a set of seen payload paths
- a set of seen file ids
- a set of seen regular-file paths usable as hardlink targets

The proof-facing bridge models one generated planner step:

```text
planner_fold_step_core(state, entry)
```

For every valid step, the proof shows that the generated Rust step:

- rejects duplicate path membership
- rejects duplicate file-id membership
- rejects a hardlink whose target is not already in the regular-path prefix
- increments the accepted-entry counter
- increments the regular-prefix counter only for regular entries

The arbitrary-list theorem:

```text
planner_fold_core_success_arbitrary
```

then proves that any arbitrary Lean list of valid generated fold steps reaches
a final fold state. This is the production-bridge layer between the fixed
five-entry manifest invariant and the real `plan_rem_tar_object` loop shape.

Boundary: this still abstracts actual `String` values and `BTreeSet` operations
as scalar membership facts (`path_seen_before`, `file_id_seen_before`,
`hardlink_target_seen_regular_before`). It does not prove Rust `Vec` iteration,
`String` ordering/equality, allocator behavior, or the `BTreeSet` implementation.
The production guardrail test
`planner_accepted_manifests_validate_against_reader_schema` exercises real
`plan_rem_tar_object` output against real `validate_manifest`.

## M9 — membership-facts bridge (`planner_membership_fold_core_success_arbitrary`)

M8 proved the planner fold once the membership facts were supplied. M9 proves
the next bridge layer: a production-source entry plus scalar facts matching the
`BTreeSet::insert` contract produces exactly those planner facts.

The proof-facing source/fact step is:

```text
planner_fold_step_from_membership_core(state, source, facts)
```

The generated extraction validates:

- `path_inserted = true` exactly when the path was not already seen
- `file_id_inserted = true` exactly when the file id was not already seen
- regular entries obey the same insert contract for the regular-path set
- accepted regular source steps require the regular-path set did not already
  contain the path
- non-regular entries do not insert into the regular-path set
- the produced planner entry carries the source path/file/link ids and the
  membership booleans unchanged

The arbitrary-list theorem:

```text
planner_membership_fold_core_success_arbitrary
```

then proves that any arbitrary Lean list of valid source/fact pairs reaches a
final fold state through the generated membership bridge.

Boundary: this proves the semantic contract that the production planner relies
on from `BTreeSet::contains`/`insert`; it still does not verify Rust's
standard-library `BTreeSet` implementation, `String` ordering/equality, real
`Vec` iteration, allocator behavior, path syntax, or exact CBOR bytes.

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties the proof-facing extraction back to
selected production snippets in `crates/remanence-format/src/layout.rs` and
`crates/remanence-format/src/manifest.rs`; if it fires, the extraction and Lean
proofs must be re-established.
