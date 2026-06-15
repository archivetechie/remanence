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
| 3 | `rem restore` naming / alias | pending |
| 4 | Cross-tree hardlink policy edge | pending |

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

## Item 3 — `rem restore` naming / alias

*(pending)*

## Item 4 — Cross-tree hardlink policy edge

*(pending)*
