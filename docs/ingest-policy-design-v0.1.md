# Ingest bundling and conformance policy — v0.1

**Status:** design approved (2026-06-12, owner); implementation deferred —
to be sequenced after the amber-merge residuals. Rem-side pieces hand to
codex via a later journal entry; ruleset authoring/profile binding is
sutradhara-side work in its own repo.

**Context:** RAO 1.0 deliberately constrains entries (canonical UTF-8
relative paths, regular files + symlinks + directories, no xattrs). Real
ingest sources violate this — non-UTF-8 legacy filenames, Mac metadata,
device nodes — and real sources also vary in *restore intent*: a curated
media project wants per-file restore; a C-drive dump wants whole-blob
safekeeping. This note defines how the ingest layer decides what becomes a
first-class RAO entry, what gets wrapped, what gets excluded, and how every
decision is recorded. It supersedes the ad-hoc "wrap non-compliant files"
guidance from the 2026-06-12 discussion.

## 1. Principles

1. **Wrap granularity = restore granularity.** The wrapper boundary defines
   the smallest unit restorable without pulling more. Choosing it is an
   *intent* declaration, not a property of the bytes — so it is explicit
   policy, never silent tool judgment.
2. **Never silent.** Every wrap, exclusion, and non-compliance decision is
   counted in the ingest report and recorded in the catalog.
3. **The future reader holds only POSIX tar.** Whatever we store must be
   self-explanatory to someone with standard tools and no remanence
   software (the RAO longevity net extends to ingest conventions).
4. **Alignment economics force blobbing for small-file swarms.** RAO
   chunk-aligns every entry's payload (RAO §4.6.3): each nonzero entry
   spans ≥ 1 chunk (256 KiB default). A million 10 KB files as individual
   entries waste ~240 GB in padding. Small-file trees go inside one tar
   payload regardless of compliance; RAO entries are for media-sized files.

## 2. The ruleset

Per-ingest policy is an ordered list of path rules, rsync-filter style.

### 2.1. Grammar

```text
<verb>  <pattern>            # comment
```

- **Verbs:** `granular`, `blob`, `exclude` (Section 2.2).
- **Patterns:** gitignore-style globs (`*`, `**`, `?`, character classes),
  matched against ingest-relative paths. A trailing `/` matches directories
  only. `blob` patterns MUST be directory patterns.
- **First match wins**, top to bottom. Rulesets read as policy statements:
  specific before general, and the last line is conventionally the
  catch-all default (`granular **` or `blob **`).
- A ruleset MAY declare `option case-insensitive` at the top (macOS
  sources). Default is case-sensitive.
- Rulesets are named, versioned files. sutradhara binds source type →
  ruleset (e.g. `fcp-project`, `premiere-project`, `photo-batch`,
  `system-dump`); `rem archive build --rules <file>` accepts the same
  format directly. The ruleset name + version (or hash) is recorded in the
  ingest report and catalog with the object.

### 2.2. Verbs

- **`granular`** — matched files become first-class RAO entries (per-file
  sha256, manifest row, PFR). Within granular regions the non-compliance
  fallback of Section 4 applies.
- **`blob <dir-pattern>`** — the matched directory becomes **one** wrapped
  entry `<path>.remwrap.tar` (Section 3). One manifest row; contents may be
  arbitrarily non-compliant; no alignment bloat. Canonical targets: macOS
  bundles (`.fcpbundle`, `.photoslibrary`, `.app`), caches, render-file
  trees, whole system dumps (`blob **` ≈ the old blob profile).
- **`exclude`** — not archived. Counted and recorded (path roots + file
  counts + total bytes) in the ingest report and the catalog's ingest
  record. Excluding is an archive-policy action; deleting junk from the
  *source* (e.g. `.fcpcache` pre-archive cleanup) remains a separate
  workflow step outside this design.

A `blob` rule **consumes its subtree whole**: rules below it never reach
inside, and v0.1 defines no carve-outs (no `granular` extraction from
inside a blobbed directory). Rationale: deliverables are siblings of cache
and bundle folders in practice, and carve-outs would make
"unwrap = exactly what was ingested" subtle. Revisit only on a concrete
case (Section 8).

### 2.3. Worked example

```text
# fcp-project.rules — ordered, first match wins
option case-insensitive

exclude    **/.fcpcache/
exclude    **/Autosave Vault/
blob       **/*.fcpbundle/
blob       **/Render Files/
granular   Final/**/*.mov
granular   **/*.fcpxml
granular   **
```

The deliverable `.mov`s and project XML stay first-class (PFR, per-file
hashes); FCP libraries and render trees ride as single restorable units;
caches are skipped with a recorded count; everything else defaults to
granular, with Section 4 handling stragglers.

## 3. The wrapper convention

1. **The wrapper is plain tar, not RAO.** RAO-in-RAO adds nothing — the
   outer object already supplies manifest, PFR, integrity, and (when
   sealed) encryption. A future tar-only reader who extracts the archive
   and finds `Render Files.remwrap.tar` knows exactly what it is and
   already holds the tool that opens it.
2. **Naming:** wrapped entry path = original directory path +
   `.remwrap.tar`. The suffix is the in-band documentation; an informative
   section in the RAO spec will document the convention as part of the
   published standard (pre-freeze, informative — no wire change).
3. **Uncompressed.** Damage tolerance and ranged reads; payloads are
   already-compressed media; RAO performs no compression by design and the
   wrapper follows suit.
4. **Fidelity flags:** created with xattr/metadata capture (pax headers via
   `--xattrs`-equivalent), preserving Mac tags, resource-fork sidecars,
   modes, and mtimes inside the wrapper.
5. **Dialect is pinned by test, not assumption.** xattr capture requires
   pax-format headers, but non-UTF-8 *path* fidelity in pax is
   implementation-murky (POSIX says pax `path` is UTF-8; GNU tar writes
   raw bytes; most readers tolerate it). Before the convention freezes,
   the chosen invocation MUST round-trip: a non-UTF-8 filename, an xattr'd
   file, a `._` AppleDouble sidecar, a dangling symlink, and an empty
   directory — byte- and metadata-exact. The winning invocation is
   recorded here and in the implementation.
6. **Restore:** `rem restore` unwraps by default — the destination tree is
   byte- and structure-identical to what was ingested (minus exclusions).
   `--no-unwrap` restores the literal stored entries. Wrap entries carry
   their own sha256 like any payload entry, so wrapped subtrees keep
   end-to-end integrity.

## 4. Non-compliance fallback inside granular regions

When a granular-matched file cannot be a native RAO entry (non-UTF-8 or
non-canonical path, unsupported type such as a device node or hardlink,
xattr-bearing where capture is demanded), the ingest layer wraps it at
**`wrap-unit = file`** (default): a one-member `.remwrap.tar` carrying the
true raw-byte name and metadata inside, named after a sanitized form of the
original path. Per-file granularity is preserved through one indirection.
`wrap-unit = dir` is available per-ruleset for sources where related
weirdness should travel together. Hardlink pairs always force `dir` at
their common ancestor (splitting them would silently duplicate data).

The pre-ingest conformance scan classifies every entry
(native / wrap-fallback / excluded) before any tape write; the report
shows counts and clustering ("214,000 of 1.1M non-compliant, concentrated
under `Users/`") so an operator can switch rulesets *before* committing.
The tool MAY suggest (e.g. "consider `blob **`") past thresholds; it MUST
NOT auto-switch.

## 5. Blob inner index (v1, optional per rule)

For each `blob` entry, the ingest layer MAY record a member index
(member path → byte offset + length within the wrapper) in the
**catalog**, sutradhara-side. Combined with outer-entry PFR this gives
ranged single-file extraction out of blobs — "one file from a blobbed
bundle, years later, without streaming the whole thing" — with zero format
changes. The index is derived state: delete-and-rebuild by re-scanning the
wrapper, never authoritative. Default on for `blob` rules unless
`blob --no-index` is set (system dumps where it's pure overhead).

## 6. Manifest annotation — the planned RAO 1.1 extension

RAO 1.0 requires the reserved manifest containers to be empty
(§4.7.2), so in-manifest wrap annotation is the textbook first **1.1**
additive extension (spec §10's designated landing zone): a
`metadata_preservation_data` key per wrapped entry carrying
`{wrapped: true, wrap_format, original_path, reason}` and an
`object_metadata` ingest record `{ruleset_name, ruleset_hash,
excluded_summary}`. Until 1.1 lands, the suffix convention + catalog
records carry the information (the suffix is what the tar-only future
actually reads anyway). The 1.1 design note is separate, deliberate work —
not to be slipped into a wire commit.

## 7. What lands where

| Piece | Home |
| --- | --- |
| Ruleset grammar + evaluation, conformance scan, wrapper creation, ingest report | sutradhara (orchestrator) + shared by `rem archive build --rules` |
| Wrap suffix convention (informative spec section) | `specs/rao-1.0-specification.md`, pre-freeze |
| Catalog ingest records (ruleset id, wrap rows, exclusion counts, blob member index) | sutradhara catalog (Layer 5 surfaces stay unchanged) |
| Restore unwrap (default) / `--no-unwrap` | rem restore path + sutradhara restore orchestration |
| RAO 1.1 manifest annotation | future spec minor-version note |

Nothing in this design changes the RAO or REM-PARITY wire formats.

## 8. Open sub-decisions

1. **Wrapper tar dialect** — pinned by the Section 3.5 round-trip test at
   implementation time.
2. **Glob dialect details** — anchoring, escaping, `**` semantics: adopt
   one existing well-specified dialect (gitignore's) wholesale rather than
   inventing; document the single deviation (first-match-wins).
3. **Sanitized wrapper names for non-UTF-8 originals** (Section 4): the
   mangling scheme (percent-encoding vs lossy + uniquifier) needs one
   deliberate choice, recorded in the spec's informative section.
4. **Suggestion thresholds** (Section 4) — operator-tunable, not normative.
5. **Carve-outs inside blobs** — out of scope for v0.1; revisit on a
   concrete demand with the unwrap-fidelity question answered first.

## 9. Out of scope

Pre-archive source cleanup (deleting `.fcpcache` from the *source*);
compression anywhere; Mac xattr capture as native RAO entry metadata (a
separate decision — wrapping carries xattrs faithfully in the meantime);
carve-outs (Section 8.5).
