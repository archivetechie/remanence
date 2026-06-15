# RAO ingest archive — deferred review items (design)

**Status:** in progress (2026-06-15, owner + claude). Resolves the four items
codex deferred from the `code-review-rao-ingest-archive-2026-06-15.md`
follow-up (commit `0e9c531`) because each needs a design/schema choice rather
than a corrective patch. Implementation hands to codex once an item is marked
**resolved** here. Source of truth for the feature:
`~/system/docs/design-ingest-v2-rao-archive.md` Part A (this refines it).

| # | Item | Status |
| --- | --- | --- |
| 1 | Non-UTF-8 member-name encoding in `.remwrap.idx` (+ customer manifest + restore request) | **Resolved** (below) |
| 2 | Cheap classification-only `--scan-only` | discussing |
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

## Item 2 — Cheap classification-only `--scan-only`

*(discussing — to be filled in)*

## Item 3 — `rem restore` naming / alias

*(pending)*

## Item 4 — Cross-tree hardlink policy edge

*(pending)*
