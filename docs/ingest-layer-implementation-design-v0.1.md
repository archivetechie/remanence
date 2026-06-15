# Ingest layer implementation — design / codex work order v0.1

**Status:** approved for implementation (2026-06-15, owner + claude). Codex
work order for `crates/remanence-cli` (`archive_ingest.rs`, `lib.rs`) and the
small `remanence-format` hooks the ingest needs. Decision record + rationale:
`ingest-archive-deferred-items-design-v0.1.md`. Pairs with the two format-level
docs: `rao-1.1-metadata-preservation-design-v0.1.md` (xattr storage) and
`rao-hardlinks-design-v0.1.md` (hardlink storage). Builds on the post-review
implementation (commit `0e9c531`).

Four work units, independently landable; order as listed.

---

## A. Member-name reversible escaping (review item 1)

**Replace lossy `from_utf8_lossy` member-name handling with a reversible
escape.** Today `build_wrap_index` / `parse_pax_records` / `tar_header_path`
substitute `U+FFFD` for non-UTF-8 bytes, which collides distinct names and
corrupts restored names. Use a single human-readable, reversible field.

**The escape rule** (applied to raw name **bytes**; deterministic):
1. literal `\` → `\\`
2. any byte not part of a valid UTF-8 sequence, plus control chars (`< 0x20`,
   `0x7F`) → `\xHH` (lowercase hex)
3. all other valid UTF-8 → passed through unchanged

Rule 1 is the bijection lynchpin (without it `a\x41b`-literal and `a`+byte-0x41
collide). Provide an encoder and a decoder; round-trip is exact.

**Where:**
- `WrapIndexEntry` name field holds the escaped form (rename `path`→`name` for
  clarity if cheap). Built from the pax `path` / ustar name bytes via the
  encoder, not `from_utf8_lossy`.
- Lookup/restore (`resolve_blob_member_from_index`, the `--blob-member` arg in
  `lib.rs`) decode **both** the stored name and the requested name to raw bytes
  and compare bytes.

**Shared contract:** the customer manifest (sutradhara) uses the identical
escaping. Document the rule once; both sides implement it.

**Tests:** round-trip clean-ASCII, valid-UTF-8 non-ASCII, Latin-1 `\xe9`,
literal-backslash, control char; a collision test proving `r\xE9sume` vs
`r\xEEsume` get distinct keys and restore byte-exact.

---

## B. Cheap classification-only `--scan-only` (review item 2)

`--scan-only` currently runs full `materialize_inputs` (hashes every file,
writes every wrapper tar, builds every `.idx`, holds every path) — a full build
minus the object write. Make it a true pre-flight.

1. **Shared classifier.** Factor the per-entry decision (`decide` ruleset match
   + `native_status` metadata checks → a verdict) into one function. Build and
   scan both call it; they diverge only in the tail — scan **records** the
   verdict (rollups: `dir_stats`, `clusters`, `totals`); build **records and
   materializes** (hash, wrap, push). This guarantees the dry run predicts the
   build exactly.
2. **Classification-only walk** for `--scan-only` (`scan_only_report` → a new
   `scan_inputs`): one `stat` per entry, update rollups, emit the report. **No
   content hashing, no wrapper-tar creation, no `.idx`, no `files`/
   `manifest_entries` population.** Memory drops to O(#directories).
3. **xattr detection via the `xattr` syscall crate**, not a `getfattr`
   subprocess per file (a million forks would dominate the scan). This removes
   the external `attr` dependency, supersedes the H4 "fail loudly if getfattr
   missing" guard, and replaces the brittle `getfattr` stdout parsing in
   `has_xattrs`. (Capture still goes through bsdtar; only *detection* moves to
   the syscall.)

**Tests:** `--scan-only` over a tree produces the same classification a build
would (assert verdicts match), writes no `.rao` and no wrapper tars, and does
no content hashing (e.g. inject a hash that would panic if called).

---

## C. xattr ingest policy (review item 2b; stores via RAO 1.1)

xattrs no longer force a wrap. Collect them, filter by policy, and route:
**small → RAO 1.1 `metadata_preservation_data` annotation on the native entry;
large → wrap** (per the 1.1 doc's size threshold, ~4 KiB/xattr + a per-file
total cap). Junk is dropped.

**Policy model (ruleset-configurable, fail-safe default):**
- **Universal junk baseline** — a built-in, rem-shipped set of always-dropped
  macOS ephemerals (`com.apple.quarantine`, `com.apple.metadata:kMDItemWhereFroms`,
  `com.apple.lastuseddate#PS`, Spotlight/FinderInfo noise). Updated as OSes
  evolve. Rulesets never re-type it.
- **Per-ruleset stance**, parsed from `option`-style directives (name-based,
  ruleset-global — not per-path):
  - `option xattr-mode denylist` (default) — keep every xattr except the
    universal baseline plus any `xattr-drop <name>` lines.
  - `option xattr-mode allowlist` — keep only the xattrs named by
    `xattr-keep <name>` lines; drop all others.
- **Absent any directive → fail-safe default**: denylist stance, baseline only.
- **Never silent:** count dropped xattrs (by name × directory-prefix cluster)
  in the scan/report, every mode.

**Ruleset grammar additions** (`load_ruleset`): accept `option xattr-mode
denylist|allowlist`, and `xattr-keep <name>` / `xattr-drop <name>` directives.
Validate (mode value; keep/drop consistent with mode). Carry into the
`ProcessContext`.

**Collection + routing** (`native_status` / `process_leaf`): a regular file's
non-junk xattrs (after the mode filter) — if all fit the size threshold, attach
them to the native entry as 1.1 metadata (hand to the format writer); if any
exceeds it, route the file to the wrap path (the tar carries the big xattr).
A file with only junk/dropped xattrs is a clean native entry (no wrap, no
annotation). **xattr presence is no longer, by itself, a `WrapFallback`.**

**Tests:** a granular file with a color-tag xattr → native entry + 1.1
annotation, restore reapplies it; a denylisted xattr → dropped, recorded,
object stays 1.0; allowlist mode drops a non-listed xattr (recorded); an
oversized xattr → file wraps.

---

## D. Hardlink ingest (review item 4; stores via native typeflag 1)

Implements the ingest half of `rao-hardlinks-design-v0.1.md`.

1. **Detect** hardlink groups by `(dev, ino)` during the classify walk (the
   `stat` is already done). A group of size > 1 → one primary + links.
2. **Primary selection** (deterministic): first group member in entry order;
   if excluded by a ruleset, the first non-excluded member; if only one
   survives, a plain regular entry.
3. **Emit** the primary as a normal native regular entry and each other name as
   a native hardlink entry (typeflag 1, `link_target` = primary's in-object
   path, carrying the primary's content coords — handed to the format writer).
4. **Edge:** a group split across a blob boundary (one name in a blobbed dir,
   one granular) can't link across it → the affected member falls back to an
   independent copy, recorded.
5. **Remove** `collect_hardlink_roots`, the common-ancestor computation, the
   cross-tree-collapse path, and the `has_multiple_hardlinks → WrapFallback`
   branch in `native_status`.

**Tests:** two granular hardlinked names → primary + typeflag-1 link in the
object; restore yields two names sharing one inode; the old whole-tree-collapse
scenario now produces a clean primary+link, not a blob; excluded-primary edge.

---

## Constraints / DoD (all units)

- `cargo fmt` / `clippy -D warnings` clean; `cargo test -p remanence-cli` green;
  the per-unit tests above exist and would fail if the behavior regressed.
- No change to the RAO/REM-PARITY wire formats beyond what the two format docs
  specify (1.1 metadata + typeflag 1); regular-only objects stay byte-identical.
- Update `docs/INDEX.md`; per `AGENTS.md`, run + paste test output and commit at
  green milestones.
