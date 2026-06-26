# Code review — `rem archive build --map` (P2.4), 2026-06-26

**Scope:** the implementation of `docs/design-rem-archive-build-map.md` (which
passed three codex review rounds) — commit `29235c2` ("Implement archive build
source maps"). New `crates/remanence-cli/src/archive_map.rs` (331 lines) + lib.rs
wiring (+917, mostly tests) + report/journal. No `remanence-library`/format/api
change; no RAO wire-format change.

**Method:** read the security-critical TSV parser (`archive_map.rs`) line by
line; dispatched a parallel review of the lib.rs wiring + tests that verified
claims against the code (ran build + clippy + the 10 map tests); independently
re-confirmed the three load-bearing bits (runtime guards, no-sort, the `--inputs`
clap attr).

**Gates (green):** `cargo fmt --check` clean; `cargo clippy -p remanence-cli
--all-targets -- -D warnings` clean; `cargo test -p remanence-cli` **118 lib
pass / 0 fail** (incl. 10 new map tests); the lane also ran full `cargo test` +
`cargo build --release` clean.

## Verdict

**Clean, faithful, security-correct — no Critical/High/Medium.** Every
load-bearing decision from the (thrice-reviewed) design is implemented as
specified, and the parser is exemplary defensive code for medium-sourced input.
codex also caught and fixed a real bug in my round-3 spec snippet (it had put
`conflicts_with = "scan_only"` on `--inputs`, which would have broken the valid
`archive build --inputs … --scan-only`; the `--map` arg correctly carries that
conflict instead). One Low + two Nits, none blocking.

## Verified correct

- **Wire-spec parsing byte-for-byte** (`archive_map.rs`): exact header constant,
  UTF-8 + trailing-LF required, 5 TAB columns, per-field control-char rejection
  (`char::is_control()` → C0/DEL/C1, incl. a stray CR), lowercase-only 64-hex
  member sha256, decimal-`u64` size, empty-map rejection, `ensure_unique_archive_paths`.
- **Raw `archive_path` validator** (`validate_archive_path_field`): splits on `/`,
  rejects empty/`.`/`..` components + leading/trailing slash — so `a//b`,
  `a/./b`, `a/../b`, `/abs`, `a/` are **rejected, not normalized** (the design's
  whole point; `Path::components()` is correctly avoided).
- **Source-path security anchor** (`validate_source_path_field`):
  `is_absolute()` → `fs::canonicalize` (resolves symlinks, so an escaping symlink
  fails the next check) → **component-wise `starts_with(canonical_root)`** (also
  immune to the `/root-evil` prefix trick) → `is_file` → `stat().len() ==
  declared_size`. The residual canonicalize→open TOCTOU is the design's accepted
  residual (trusted producer, single-tenant); the writer's sha256 recompute +
  exact-size read catch any content swap anyway.
- **Deterministic `file_id`** via `deterministic_archive_entry_file_id` (the same
  helper `--inputs` uses) — not a random UUID; needed for byte-reproducibility.
- **`--map-sha256`** checked **before** parsing rows/opening sources; parses the
  **first whitespace token** so the producer's BagIt `manifest-sha256.txt`
  (`<hex>␣␣file`) works.
- **CLI wiring:** the 3 flags are in **both** `RemArchiveBuildArgs` (`:1238`) and
  `ArchiveBuildArgs` (`:1809`) + carried through the `From` (`:1614`); `--map`
  has `conflicts_with_all = ["inputs","rules","scan_only"]` + `requires =
  "source_root"`; `--inputs` has `required_unless_present_any = ["scan_only",
  "map"]` and **no** `conflicts_with="scan_only"`. Correct multi-arg clap methods.
- **Runtime defence-in-depth** (`build_archive_object_file:5522`): rejects
  `map`+`scan_only`/`rules`/`inputs`/missing-`source_root`, and
  `map_sha256` without `map` — not relying on clap alone for the security guard.
- **No-sort / map order:** the map plan's inputs come straight from
  `load_source_map` (TSV order); the two sorts (`--inputs` `lib.rs:7281`,
  `--rules` `archive_ingest.rs:330`) are not on the map path. Proven by the
  map-order test (strictly increasing `first_chunk_lba`).
- **`ingest_item_id`** added to `ArchiveBuildInputFile` as `Option<String>`; all
  non-map construction sites set `None`; the report emits it **verbatim as a JSON
  string**. Top-level **`map_sha256`** is distinct from the pre-existing
  `manifest_sha256` (the RAO internal hash).
- **`--manifest-out` with `--map`** allowed (runtime gate relaxed) — emits the
  customer manifest from map rows (`tar_engine.program == "source-map"`).
- **`Box<ArchiveCommand>`** is a mechanical boxing to silence
  `clippy::large_enum_variant` after the arg structs grew — dispatch unchanged,
  tests green. No panic/unwrap on map-derived data (the two `tuning.expect`s are
  unreachable when `--map` is set).
- **Tests** cover all DoD items: round-trip + map order, sha256/size mismatch
  fail-closed (no object), source escape / relative / symlink-escape, raw
  archive_path cases, duplicate path, the malformed-TSV matrix (col count /
  control / uppercase hex / non-decimal / bad header / missing newline /
  non-UTF-8), `--map` w/o `--source-root`, `--map --scan-only`, `--map-sha256`
  mismatch, encrypted round-trip, byte-reproducibility (pinned
  object-id+caller+manifest-file-id+timestamp), scan-only regression kept green.

## Findings (none blocking)

- **L1 (Low) — duplicate `manifest_out` set in the report.** `archive_build_report_json`
  sets `report["manifest_out"]` in both the `ingest` block (`lib.rs:7181`) and the
  `map_sha256` block (`:7187`). The two are mutually exclusive per build and the
  value is identical, so it's harmless; hoisting the single set out of both would
  be cleaner. Optional.
- **Nit — no bare `--inputs --scan-only` (without `--rules`) test.** The
  regression the design worried about is structurally impossible now (the attr is
  absent — verified), and scan-only tests exercise `--inputs` alongside `--rules`;
  a dedicated no-`--rules` case would be belt-and-braces. Optional.
- **Nit — `--map-sha256` tolerates uppercase hex** (in-row `sha256` is
  lowercase-only). Intentional: `--map-sha256` parses the user-supplied/BagIt
  digest, where tolerating uppercase is reasonable. No action.

## Scope (as designed)

Ends at the verified RAO **object file + report**. Getting it onto tape and
recording `Copy`/`AssetLocator` rows is **P2.5** (sutradhara + `~/system`), keyed
on the report's per-member `ingest_item_id` (string) — the cross-repo end-to-end
gate.

## Net

P2.4 is complete and ready. The arrangement arc's rem-side consumer faithfully
matches sutradhara's committed producer (the wire-spec held byte-for-byte across
three review rounds), reads originals in place (no staging copy), verifies every
member, and preserves arranged order. Hand to P2.5 for the tape-write +
locator-recording loop.
