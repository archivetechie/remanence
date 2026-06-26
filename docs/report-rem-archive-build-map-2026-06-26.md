# Rem Archive Build Map Implementation Report

Date: 2026-06-26
Author: codex
Design: `docs/design-rem-archive-build-map.md`

## Summary

Implemented the rem-side P2.4 source-map consumer:
`rem archive build --map <source-map.tsv> --source-root <dir>` now builds a
RAO object directly from arranged member rows without a copied staging tree.

The change is a new input frontend over the existing archive build core. It
does not change the RAO wire format, catalog schema, tape write path, or system
side.

## Implemented

- Added `crates/remanence-cli/src/archive_map.rs` to parse and validate the
  source-map TSV contract:
  exact header, UTF-8, TAB/LF with trailing newline, five columns, no control
  characters, lowercase 64-hex member SHA-256, decimal `u64` size, and duplicate
  `archive_path` rejection.
- Added raw `archive_path` validation by splitting on `/`; `a//b`, `a/./b`,
  `a/../b`, `/abs`, and trailing slash paths are rejected instead of normalized.
- Added `source_path` guards: absolute path required, canonical source-root and
  source path, canonical containment check, regular-file check, and `stat ==
  size` check before build.
- Added optional `--map-sha256`; it checks the TSV byte digest before parsing
  rows or opening source files. The parser accepts the first whitespace-delimited
  token so BagIt-style manifest lines work.
- Added `--map`, `--source-root`, and `--map-sha256` to both build arg structs
  and the `From<RemArchiveBuildArgs> for ArchiveBuildArgs` conversion.
- Preserved map row order into the writer; member `file_id`s are deterministic
  using the existing archive entry id helper.
- Added per-member `ingest_item_id` to build report rows and top-level
  `map_sha256` to map build reports.
- Allowed `--manifest-out` with map builds by emitting the existing customer
  manifest shape with source-map-derived regular entries.
- Boxed the internal `Command::Archive` subcommand to keep clippy's enum-size
  lint green after the build arg growth.

## Tests

Added CLI-level tests for:

- plaintext map build round-trip, map order, first-chunk ordering,
  `map_sha256`, `ingest_item_id`, and map-derived manifest output;
- malformed maps: wrong column count, control characters, uppercase/bad hex,
  non-decimal size, duplicate archive path, bad header, missing trailing newline,
  and non-UTF-8 map bytes;
- raw archive-path rejection without normalization;
- absolute source escape, relative source path, and symlink escape;
- runtime/clap guards for missing `--source-root` and `--map --scan-only`;
- `--map-sha256` mismatch before source validation;
- size mismatch and streamed payload hash mismatch fail closed with no final
  object;
- encrypted map build and extract round-trip;
- byte reproducibility with pinned object id, caller id, manifest file id, and
  timestamp.

Existing scan-only tests were kept green; the design snippet was corrected so
`--inputs` remains valid with `--scan-only` while `--map` conflicts with both.

## Verification

Focused checks already run:

```text
cargo test -p remanence-cli archive_build_map -- --nocapture
cargo test -p remanence-cli archive_build_rules_scan_only -- --nocapture
cargo clippy -p remanence-cli --all-targets -- -D warnings
cargo test
```

Full required gates were run after this report was added; see the final summary
for exact command output.
