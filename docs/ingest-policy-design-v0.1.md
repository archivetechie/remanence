# Ingest bundling and conformance policy — v0.1

**Status:** design approved (2026-06-12, owner); **refined 2026-06-14**
(brainstorm with Claude — see "Refinements" below). The amber-merge residuals
that gated sequencing are now done (the RAO sealing migration landed). Rem-side
pieces hand to codex via a later prompt; ruleset authoring/profile binding is
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

## Refinements (2026-06-14, brainstorm)

Decisions locked while re-cutting ingest-v2 Phase S onto this model. They edit
§2/§3/§7 below; recorded here as the changelog.

1. **Verbs are `blob` and `exclude` only; `granular` is the implicit default
   *state*, not a verb.** Any path not matched by a `blob`/`exclude` rule is
   granular (a first-class RAO entry). Drops the redundant trailing
   `granular **` from rulesets and removes `granular` from the grammar.
   (§2.1, §2.2)
2. **Unmatched path → granular by default; the conformance-scan review is a
   mandatory gate.** No catch-all is required for totality — granular is the
   floor. "Never silent" (§1.2) is satisfied by the scan surfacing what the
   floor and fallbacks will do (incl. alignment-bloat swarms, §1.4), not by
   forcing a catch-all. An explicit `blob **` / `exclude **` last line only
   sets a *different* floor. (§2.1, §4)
3. **Re-include deferred.** The one thing `blob`+`exclude` can't express is
   re-including a file from under a broader `exclude` (rsync's
   include-before-exclude idiom). Deferred like blob carve-outs (§8.5); add an
   explicit re-include verb only on concrete demand — purely additive.
4. **First-match-wins retained, deliberately.** Kept over gitignore's
   last-match-wins: it matches the closest domain peers (rsync and borg are
   both first-match; restic is the gitignore-style outlier) and reads as an
   ordered policy for these expert-authored, versioned, scan-gated rulesets.
   Documented as the deliberate deviation from gitignore. The conformance scan
   lints **unreachable rules** (a rule whose match-set is subsumed by an
   earlier broader one — the characteristic first-match-wins footgun; cheap
   cases to detect: a catch-all that isn't last, exact duplicates, a literal
   sub-path of an earlier directory/blob pattern). (§2.1, §8.2)
5. **Wrapping is mechanism, owned by remanence; sutradhara is policy.** The
   ruleset engine + wrap/unwrap is the canonical `rem archive build --rules` /
   `rem restore`, so `rem` is a complete standalone archive tool. The RAO
   *format* crate stays oblivious (a `.remwrap.tar` is an opaque regular-file
   entry). sutradhara contributes only the source-type→ruleset binding,
   copy/lifecycle policy, and catalog records — it *calls* `rem --rules`, never
   reimplements wrapping. (§3, §7)
6. **The wrapper writer is a pinned mainstream tar engine, never a bespoke
   codec.** The "future reader holds only POSIX tar" guarantee (§1.3) is only
   trustworthy if the bytes are what GNU tar / bsdtar (libarchive) actually
   write, so `rem` shells out to / FFI-links a pinned engine; the specific
   engine is pinned by the §3.5 round-trip test (lean: bsdtar/libarchive for
   xattr + AppleDouble fidelity and Mac/Linux parity — still open). (§3, §8.1)

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

- **Verbs:** `blob`, `exclude` (Section 2.2). `granular` is the implicit
  default *state* for any unmatched path, not a verb.
- **Patterns:** gitignore-style globs (`*`, `**`, `?`, character classes),
  matched against ingest-relative paths. A trailing `/` matches directories
  only. `blob` patterns MUST be directory patterns.
- **First match wins**, top to bottom (deliberate — matches rsync/borg, the
  closest domain peers; the documented deviation from gitignore's
  last-match-wins). Rulesets read as ordered policy statements: specific
  before general. No catch-all is required — an unmatched path is granular by
  default (§2.2); an explicit `blob **` / `exclude **` last line only sets a
  *different* floor. The conformance scan lints **unreachable rules** (one
  whose match-set is subsumed by an earlier broader rule — the first-match
  footgun).
- A ruleset MAY declare `option case-insensitive` at the top (macOS
  sources). Default is case-sensitive.
- Rulesets are named, versioned files. sutradhara binds source type →
  ruleset (e.g. `fcp-project`, `premiere-project`, `photo-batch`,
  `system-dump`); `rem archive build --rules <file>` accepts the same
  format directly. The ruleset name + version (or hash) is recorded in the
  ingest report and catalog with the object.

### 2.2. Verbs

- **Default state — `granular`** (not a verb): any path not matched by a
  `blob`/`exclude` rule becomes a first-class RAO entry (per-file sha256,
  manifest row, PFR). The non-compliance fallback of Section 4 applies to
  these default (granular) regions.
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
# no catch-all needed — everything else (the deliverable .movs, .fcpxml, …)
# is granular by default
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
   recorded here and in the implementation. The writer is a **pinned
   mainstream tar engine** (GNU tar or bsdtar/libarchive), invoked or
   FFI-linked by `rem` — never a bespoke Rust pax codec, so the §1.3
   longevity guarantee holds against the actual mainstream reader.
6. **Restore:** `rem restore` unwraps by default — the destination tree is
   byte- and structure-identical to what was ingested (minus exclusions).
   `--no-unwrap` restores the literal stored entries. Wrap entries carry
   their own sha256 like any payload entry, so wrapped subtrees keep
   end-to-end integrity.

## 4. Non-compliance fallback in default (granular) regions

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
NOT auto-switch. This scan-and-review is a **mandatory gate**, not advisory:
it is how "never silent" (§1.2) is satisfied under the granular-by-default
floor — the operator sees what the floor and fallbacks will do (including the
§1.4 alignment-bloat swarms) before any commit.

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
| Ruleset eval, conformance scan, wrap/unwrap creation — the **mechanism** | **remanence**: canonical `rem archive build --rules` / `rem restore` (a higher rem crate; the RAO *format* crate stays oblivious). `rem` is a complete standalone archive tool. |
| Source-type → ruleset binding, copy/lifecycle policy, ingest-report presentation — the **policy** | sutradhara: calls `rem --rules`, never reimplements wrapping |
| Wrap suffix convention (informative spec section) | `specs/rao-1.0-specification.md`, pre-freeze |
| Catalog ingest records (ruleset id, wrap rows, exclusion counts, blob member index) | sutradhara catalog (Layer 5 surfaces stay unchanged) |
| Restore unwrap (default) / `--no-unwrap` | rem restore path + sutradhara restore orchestration |
| RAO 1.1 manifest annotation | future spec minor-version note |

Nothing in this design changes the RAO or REM-PARITY wire formats.

## 8. Open sub-decisions

(Verb set, granular-as-default, ordering, unmatched-path semantics, and the
mechanism/policy split are now resolved — see Refinements. Remaining:)

1. **Wrapper tar dialect** — pinned by the Section 3.5 round-trip test at
   implementation time (lean: bsdtar/libarchive; see Refinements #6).
2. **Glob *matching* dialect** — anchoring, escaping, `**` semantics: adopt
   gitignore's glob matching wholesale, but with first-match-wins precedence
   (locked — Refinements #4) as the documented deviation, plus the
   unreachable-rule lint described there.
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
