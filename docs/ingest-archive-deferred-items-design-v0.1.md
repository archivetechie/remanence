# RAO ingest archive — deferred review items (design)

**Status:** in progress (2026-06-15, owner + claude). Resolves the four items
codex deferred from the `code-review-rao-ingest-archive-2026-06-15.md`
follow-up (commit `0e9c531`) because each needs a design/schema choice rather
than a corrective patch. Implementation hands to codex once an item is marked
**resolved** here. Source of truth for the feature:
`~/system/docs/design-ingest-v2-rao-archive.md` Part A (this refines it).

| # | Item | Status |
| --- | --- | --- |
| 1 | Non-UTF-8 member-name encoding in `.remwrap.idx` (+ customer manifest + restore request) | **Resolved** |
| 2 | Cheap classification-only `--scan-only` | **Resolved** |
| 2b | xattr handling policy (pulled RAO 1.1 forward) | **Resolved** (own doc) |
| 3 | `rem restore` naming → foreign formats become plugins (BRU out of core) | **Resolved** (direction; own design doc to follow) |
| 4 | Cross-tree hardlink edge → flatten hardlinks | **Resolved** |

---

## Item 1 — Member-name encoding: reversible escaping (RESOLVED)

### Problem

A `.remwrap.tar` member can have a non-UTF-8 name (legacy 8-bit encodings off
old Windows/Mac disks — e.g. `résumé.doc` stored Latin-1, bytes
`72 E9 73 75 6D E9 2E 64 6F 63`, where `E9` is not valid UTF-8). Such files are
*why* wrapping exists: RAO native entries require clean UTF-8 paths, so these
go inside a tar, where bsdtar/pax preserves the raw bytes exactly.

The `.remwrap.idx` (the on-tape sibling entry mapping member → offset/len/sha256
for single-file restore) is built by parsing the tar headers through
`String::from_utf8_lossy` (`build_wrap_index` / `parse_pax_records` /
`tar_header_path`). That substitutes the replacement char `U+FFFD` (�) for each
bad byte and **discards the original bytes**. Two real failures:

1. **Collisions → wrong file restored.** `r\xE9sume.doc` and `r\xEEsume.doc`
   both collapse to `r�sume.doc`; the idx then has two entries with the same
   key and a single-file restore can return the wrong file's bytes — silent
   corruption.
2. **Mangled restored name.** A single-file restore through the idx can only
   recreate the file as `r�sum�.doc` (replacement chars baked in), not the
   original — defeating the fidelity that wrapping exists to provide.

A **full unwrap** is unaffected (it goes through bsdtar, which has the raw
bytes); only the idx-mediated single-file path — the whole reason the idx
exists — is broken.

### Decision: one `name` field, reversibly escaped

Store each member name in a **single human-readable field that is also
losslessly reversible** — no base64 second field, no lossy `U+FFFD`. Render
non-representable bytes the way `ls`/`cat -v` do, with backslash-hex escapes.
The escaped string is both what a human reads and the canonical key the
machine matches on.

**Why not the alternatives** (the path we reasoned through):
- *Lossy UTF-8 (status quo):* not reversible → the two failures above.
- *base64-only for bad names:* lossless but a pure base64 blob tells a human
  nothing about a file they may want to identify — and these are exactly the
  files someone needs to identify.
- *Escaped display + base64 canonical (two fields):* redundant. If the escaping
  is reversible, the base64 carries no information the escaped form lacks.
- *Reversible escaping (chosen):* readable everywhere **and** lossless in one
  field.

### The escaping rule (normative; must be specified exactly)

Because the escaped string is now the canonical identifier (not just display),
the rule must be deterministic and reproducible across implementations and
decades — the idx is a rebuildable projection of the tar, and the customer
manifest must be reproducible. Applied to the raw name **bytes**:

1. A literal backslash `\` → `\\`.
2. Any byte that is **not part of a valid UTF-8 sequence**, plus **control
   characters** (`< 0x20` and `0x7F`) → `\xHH` (lowercase hex).
3. Every other byte (all remaining valid UTF-8) → passed through unchanged.

Rule 1 is the reversibility lynchpin: without escaping the escape char,
`a\x41b` (six literal chars) and `a`+byte `0x41`+`b` both render to `a\x41b` —
ambiguous. With it they differ (`a\\x41b` vs `a\x41b`), giving a true
bijection: every byte string ↔ exactly one escaped string, decodable with only
the three rules above.

Rule 2 deliberately does **not** judge "is this character printable" (which is
Unicode-version-dependent). Valid UTF-8 passes through except control chars; the
only escapes are invalid bytes, control chars, and backslash. That keeps the
mapping stable for the lifetime of the archive.

Examples:
- `report.pdf` → `report.pdf` (unchanged)
- UTF-8 `résumé.doc` → `résumé.doc` (valid UTF-8, unchanged)
- Latin-1 `résumé.doc` (bytes `…E9…E9…`) → `r\xe9sum\xe9.doc`
- a name containing a literal `\` and a tab → `\\` and `\x09`

### Scope — one identifier across three artifacts

The same escaped form is the member identifier in all three places, so they
agree and round-trip:
- **`.remwrap.idx`** (rem) — the `name` field; lookups decode escapes → raw
  bytes and match byte-for-byte.
- **Customer manifest** (sutradhara) — same escaped names, so the customer can
  read their listing and copy a name to request it.
- **Restore request** (`rem … --blob-member <name>`) — accepts the escaped
  form; the tool decodes it to bytes before lookup. (Bonus: the escaped form is
  ASCII and shell-typeable; base64 would also be, but the escaped form is
  recognizable.)

### Implementation pointers (rem)

- Replace the `from_utf8_lossy` name handling in `build_wrap_index`,
  `parse_pax_records`, and `tar_header_path`
  (`crates/remanence-cli/src/archive_ingest.rs`) with the escape encoder; add a
  matching decoder.
- `WrapIndexEntry.name` (renamed from `path`) holds the escaped form;
  `CustomerManifestEntry` likewise.
- Lookup/restore (`resolve_blob_member_from_index` and the `--blob-member`
  parse in `crates/remanence-cli/src/lib.rs`) decode both the stored name and
  the requested name to raw bytes and compare bytes.
- Unit tests: round-trip the rule on (a) clean ASCII, (b) valid-UTF-8 non-ASCII,
  (c) Latin-1 `\xe9`, (d) a literal-backslash name, (e) a control char; and a
  collision test proving `r\xE9sume` and `r\xEEsume` get distinct keys and
  restore to distinct, byte-exact names.

### Cross-repo

Sutradhara's customer-manifest emitter must use the identical escaping so the
member identifier is consistent end to end. Coordinate when the manifest format
is finalized.

---

## Item 2 — Cheap classification-only `--scan-only` (RESOLVED)

### Problem

`--scan-only` calls the same `materialize_inputs` a real build calls, so the
"dry run" actually: SHA-256-hashes **every** native file (full content reads),
shells to bsdtar and **writes a `.tar`** for every blob and fallback wrap (then
hashes each and builds its `.idx`), and holds one record per entry in RAM
(`files` + `manifest_entries`). On a 1 TB messy tree that's hours of I/O and a
pile of temp disk — a full build minus the object write, not a pre-flight.

### Decision

A **classification-only walk** for `--scan-only`: per entry run `decide`
(ruleset) + `native_status` (metadata-level checks), update only the rollups
(`dir_stats` for density, `clusters`, `totals`), and emit the report. **No
content hashing, no tar creation, no `.idx`, no `files`/`manifest_entries`
population.** Cost is one `stat` per entry (no content I/O); memory drops to
O(number of directories).

Two supporting requirements:

1. **Shared classifier.** The per-entry classification (`decide` +
   `native_status` → verdict) MUST be a single function both the scan and the
   build call, so a dry run provably predicts the build. They diverge only in
   the tail: scan records the verdict; build records *and* materializes (hash,
   wrap, push).
2. **xattr detection via the `xattr` syscall crate**, not a `getfattr`
   subprocess per file (a million forks would dominate the scan). This also
   removes the external `attr` dependency and supersedes the H4 "fail loudly if
   getfattr missing" guard — there's no external tool left to be missing. See
   the xattr policy section and the 1.1 doc.

### Implementation pointers (rem)

- `crates/remanence-cli/src/archive_ingest.rs`: split the walk so
  `process_leaf`/`process_dir` compute the classification once via a shared
  function; `materialize_inputs` keeps the materializing tail, a new
  `scan_inputs` (used by `scan_only_report`) keeps only the recording tail.
- Replace `has_xattrs`' `getfattr` shell-out with `xattr::list()`.

## xattr handling policy (RESOLVED — pulls RAO 1.1 forward)

xattrs no longer force a wrap. The model:

- **Junk denylist** (`com.apple.quarantine`, `…WhereFroms`,
  `…lastuseddate#PS`, Spotlight/FinderInfo noise — tunable) → dropped
  silently, never affects classification. These are the inadvertent ones macOS
  sprinkles (Gatekeeper etc.); treating them as significant would mass-wrap or
  spuriously halt `expect=compliant` bundles.
- **Meaningful xattrs** (everything not denylisted — e.g. Finder color tags
  `com.apple.metadata:_kMDItemUserTags`) → **preserved on a clean native entry
  via the RAO 1.1 `metadata_preservation_data` annotation** when small, or via
  a **wrap** when large (> threshold; the resource fork is the only routinely
  large case and is near-absent in modern media). Detection via the `xattr`
  syscall crate.

This pulls **RAO 1.1 forward** (1.0 isn't frozen; the manifest already
reserves the container; 1.0 readers already tolerate it; doing it now exercises
the additive 1.x mechanism for real before freeze). Full design:
**`rao-1.1-metadata-preservation-design-v0.1.md`**.

**AppleDouble (`._` sidecars):** rem does **not** transcode `._` ↔ xattr — it
stays a faithful byte engine (a `._foo` arriving as a file is archived as a
file and restored as a file; macOS re-merges sidecars on its own end). The
*optional* normalization — merging `._` sidecars into native xattrs so the
annotation captures them uniformly regardless of transport — belongs in
**sutradhara** as a recorded, opt-in, staging-time transform on the staged
copy (before `rem archive build`). Because rem only ever sees native xattrs,
its contract is unchanged whether they arrived natively or via sutradhara's
merge. Cross-repo: specify in the sutradhara design (tooling: Netatalk-style
AppleDouble handling on Linux; `dot_clean` is macOS-only). Caveat: merging a
big resource fork makes a big xattr → the file then wraps (lossless); and an
`exclude **/._*` rule must never run on un-merged Case-B data or it silently
drops the metadata.

**Open sub-decision (small):** denylist (preserve every non-junk xattr) vs.
allowlist (keep only known-meaningful, e.g. color tags, drop the rest). Lean
**denylist** — the don't-lose-data ethic — with the drop list tunable.

## Item 3 — Foreign formats become plugins; native restore gets the clean verb (RESOLVED — direction)

The naming collision (`rem archive restore` = legacy BRU dump vs. `archive
extract` = RAO, with the design wanting `rem restore`) is a symptom. The real
issue: **BRU shouldn't be in rem core at all.** There are two format
categories — **native** (RAO plain/aead; rem *writes* these; core) and
**foreign/legacy** (BRU, old tar, …; rem only *reads* them to migrate off old
tapes; reverse-engineered; inherently per-deployment — archive has BRU, another
site has something else). Baking one organization's legacy into the core tool
is wrong.

**Decision:** foreign formats are **plugins**. BRU becomes its own project
implementing rem's published foreign-format-driver trait (sketched in
`format-driver-streaming-boundary.md`); rem core ships with **zero** foreign
formats; a deployment assembles its `rem` binary from core + the plugins it
needs (**compile-time** — distribution crate / feature flags — *not* dynamic
loading, per the note and to avoid Rust's no-stable-ABI pain).

This dissolves the naming collision at the root: with the BRU-specific
`archive restore` command gone from core, `rem restore` (or `archive extract`)
cleanly owns native RAO restore, and foreign restore is the generic
`rem archive <op> --format <plugin>` dispatch (already designed in the note),
present only when a plugin is compiled in.

**Scope:** its own architecture work item — promote the driver trait to a
published extension point; move `remanence-bru` out of the core workspace;
plugin-gate the `--format` dispatch; assemble the binary from core + plugins.
Bigger than naming; pairs with `tape-platform-seam-design-v0.1.md` (foreign
read-formats are the format-layer complement to the platform seam's
layout/catalog reuse). **To be written up as its own design doc.**

**Sub-decisions for that write-up:** (a) linked-in crate (lean) vs
out-of-process subprocess plugin; (b) where the driver-trait crate lives
(new `remanence-format-driver` vs platform/library layer); (c) incremental
(feature-flag BRU out now, separate repo later) vs big-bang.

## Item 4 — Hardlinks: flatten, don't preserve-via-wrap (RESOLVED)

The "cross-tree collapse" (one hardlink pair spanning two subtrees blobs the
whole input) is a self-inflicted edge: it exists *only* because the design
preserves hardlinks by wrapping their **common-ancestor directory** (to borrow
tar's hardlink record). For cross-subtree links the common ancestor is the
root → the whole archive becomes one blob. The mechanism is fighting RAO 1.0's
own scope, which **deliberately has no hardlink entry type**.

**Decision: flatten hardlinks.** Each hardlinked name becomes an ordinary
**native RAO entry** with its own copy of the bytes. `nlink > 1` is no longer a
wrap-fallback trigger. Rationale:

- **No data is lost** — every byte is archived (stored once per name). What's
  dropped is the *inode link*, a source-filesystem storage property, not
  archival data. The archive's contract ("files back, byte-correct") is met.
- **"Same content" is still captured** — RAO records each file's sha256 and the
  catalog keys assets by it, so two hardlinked names appear as two paths with
  one identical hash. Content-addressing already says what the hardlink said.
- **Never silent** — record "N hardlink sets flattened to independent copies"
  in the scan/report.

**Tradeoff:** a hardlinked set is stored once per name (duplication). Negligible
for rare media hardlinks. If it ever matters: dedup by sha256 at bundle build
(sutradhara), or add a real RAO hardlink entry-type as a deliberate future
minor — never the directory-blob hack.

**Removes:** `collect_hardlink_roots` + its second tree-walk, the
common-ancestor computation, the cross-tree-collapse edge, and the
climb-capping band-aids. `native_status` drops the
`has_multiple_hardlinks → WrapFallback("hardlink")` branch. Net simplification.
