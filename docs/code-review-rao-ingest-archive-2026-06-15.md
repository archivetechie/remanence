# Code review — RAO ingest archive (rem side), 2026-06-15

**Scope:** the implementation of `docs/prompt-rao-archive-rem.md` (Part A of
`~/system/docs/design-ingest-v2-rao-archive.md`) — commits `e7b9e5f..4ca69c3`:
the new `crates/remanence-cli/src/archive_ingest.rs` (~2,000 lines) and the
`rem archive build/extract` wiring in `crates/remanence-cli/src/lib.rs`
(~1,160 lines). Reviewed against the prompt (scope items 1–6) and the design
(§A2–A5) as normative; the design wins over the code.

**Reviewers:** claude (the three security/correctness cores — hand-rolled glob,
shelled tar engine, tar-header index parser — line by line) plus three parallel
lanes (CLI surface + restore; scan/planner/clustering; test/DoD coverage).
Gates at review time: `cargo fmt --check` clean, `clippy -p remanence-cli
-D warnings` clean, `cargo test -p remanence-cli` **87 passed / 0 failed**.

## Verdict

The happy path of all six scope items works and is genuinely tested:
ruleset parse + first-match-wins + glob; `blob`/`exclude`; `.remwrap.tar`
wrapping (blob, file-fallback, hardlink-forces-dir); on-object `.remwrap.idx`;
the clustered conformance scan; `--manifest-out`; and restore with
default-unwrap, `--no-unwrap`, and **blob single-file ranged extract proven
both keyless and keyed**. No RAO/REM-PARITY wire-format change — it is a pure
CLI-layer addition, as required. The glob matcher is memoized (no ReDoS), the
extract path validates member paths against traversal, and the keyed/keyless
blob-member ranged extract is a real byte-comparing test. Structure is clean
and clippy passes.

**No Critical findings, but one High is RCE-class** (tar option injection via
crafted names) and would be **Critical for a service ingesting third-party
data**. The other Highs: a panic reachable from a malformed `.remwrap.tar`,
the gating §A3.5 fidelity test silently not running in CI, a silent
xattr-data-loss path, and the scan's density heuristic not implementing the
design's signature behavior. None block the clean media-card path; all block
calling this production-ready for messy/untrusted ingest.

---

## High

### H1 — Tar option injection via crafted file/directory names (`create_wrapper_tar`)
`archive_ingest.rs:1268-1278`. The wrapper is created with
`tar -c --format pax --xattrs -f <output> -C <base_dir> <member>` and **no
`--` before the positional `member` operand**. Both GNU tar and bsdtar parse
options anywhere in the argument vector, so a `member` beginning with `-` is
interpreted as an option — including GNU tar's `--checkpoint-action=exec=…`
(arbitrary command execution) and `--to-command`, and bsdtar's `--options`.
`member` is derived from the **source tree**: `relative.as_os_str()` for blobs
(a directory matched by a `blob` rule — e.g. a dir named
`--checkpoint-action=exec=sh… .fcpbundle` matches `*.fcpbundle/`) and
`path.file_name()` for file-fallback wraps. This is exactly the untrusted,
"messy source" input the feature exists to handle.
**Severity:** High here (exploitation needs an attacker-influenced name that
either matches a `blob` rule or rides on an already-non-compliant file);
**Critical** if remanence ever ingests third-party trees.
**Fix:** insert `--` immediately before the operand:
`…arg("-C").arg(base_dir).arg("--").arg(member)`. Add a test that wraps a
directory and a file whose names begin with `--` and asserts correct
archival, not option interpretation. (Extract is safe — its only operands are
`-f`/`-C` option-arguments, not bare positionals.)

### H2 — Panic on a malformed pax record (`parse_pax_records`)
`archive_ingest.rs:1520`:
`let record = &data[cursor + space + 1..cursor + len - 1];`. The guard above
ensures `len >= 1` and `cursor + len <= data.len()`, but **not**
`len >= space + 2`. A crafted record such as `"01 x\n"` (digits `01` → len 1,
space at index 2) produces the slice `data[3..0]` → start > end → **panic**.
`parse_pax_records` runs from `build_wrap_index`, which runs from
`validate_wrap_tar_paths` on the **restore/extract path** against a
`.remwrap.tar` read back from tape — so a corrupted or hostile wrapper crashes
the tool rather than erroring cleanly, violating the project's
"no panic from medium-sourced bytes" posture.
**Fix:** before slicing, require `len >= space + 2` (and that the record ends
in `\n`); on violation, stop parsing (return what's parsed) or surface a typed
parse error. Add a unit test feeding `"01 x\n"` and other `len ≤ space+1`
records and asserting no panic.

### H3 — The gating §A3.5 fidelity test never runs in CI (silent skip)
`archive_ingest.rs:1931-1938`. The round-trip test guards on
`command_available("bsdtar") && "setfattr" && "getfattr")` and otherwise
`eprintln!` + `return` — a silent pass. CI (`.github/workflows/ci.yml`)
installs `libarchive-tools` (bsdtar) but **not** `attr` (setfattr/getfattr),
so the single most important DoD deliverable early-returns on every CI run and
counts green while asserting nothing.
**Fix:** add `attr` to the CI apt install; convert the skip to a hard failure
(or `#[ignore]` + a dedicated CI job that runs `--ignored`) so an absent tool
can never read as a pass.

### H4 — Silent xattr data loss when `getfattr` is absent (`has_xattrs`)
`archive_ingest.rs:1250`. `has_xattrs` shells to `getfattr`; if the binary is
missing it returns `false`, so xattr-bearing files are classified **native**
and stored **without their xattrs** — silent loss, the precise failure §A1.2
("never silent") forbids. This bites in CI (no `attr`) and on any production
host lacking `attr`.
**Fix:** detect the xattr tool once at startup; if unavailable, **fail or warn
loudly** rather than silently reporting "no xattrs". Better: read xattrs via a
syscall crate (e.g. `xattr`) instead of shelling to `getfattr`, removing the
external dependency entirely. Add a build-pipeline test (with `attr` installed)
that ingests a granular xattr file and asserts it becomes a `.remwrap.tar`
(`reason=xattr`) restoring to xattr-identical.

### H5 — Scan density rule omits "topmost dense dir, don't climb past a compliant subtree" (`scan_report`)
`archive_ingest.rs:1681-1722`. The code emits a blob suggestion for **every**
directory independently crossing `(ratio ≥ R, count ≥ N)`, with no
topmost-only pruning and no check against swallowing a substantial compliant
sibling. The design's signature example fails: with `Users/bob/AppData/`
1000/1000 noncompliant and clean sibling `Users/bob/Documents/` 50/50,
`Users/bob/` rolls up to 0.952 ≥ 0.9 and is suggested as a blob — climbing
past the compliant `Documents/` — and `AppData/` is *also* suggested (nested
duplicates). The scan only suggests (a human/sutradhara confirms), so this is
suggestion quality, not silent action — but the core heuristic doesn't match
§A4.
**Fix:** after collecting dense candidates, keep only the topmost in each
ancestor chain, and stop the upward climb at any directory that subsumes a
substantial-and-compliant subtree. Unit-test the AppData/Documents asymmetric
case and the nested-duplicate case.

---

## Medium

### M1 — Tar dialect not pinned; production accepts an un-fidelity-tested engine
`detect_tar_engine` (`:1354`) prefers bsdtar but falls back to GNU `tar`, and
the §A3.5 test only runs against bsdtar (H3). The design requires **pinning one
dialect** because non-UTF-8/AppleDouble/xattr pax fidelity differs between
engines. As written, a host with only GNU tar uses an invocation whose
fidelity was never proven. **Fix:** either pin to bsdtar (error if absent), or
run the §A3.5 test against both engines and prove both pass; record the pinned
choice in the spec's informative section.

### M2 — `.remwrap.idx` records lossy member paths for non-UTF-8 names
`parse_pax_records` (`:1522-1523`) and `tar_header_path` (`:1546`) build member
paths via `String::from_utf8_lossy` → U+FFFD substitution. A non-UTF-8 member
inside a blob is therefore keyed in the `.idx` by a corrupted path, so the
"ranged single-file restore from a blob" path (§A5) cannot locate exactly the
files wrapping exists to preserve. **Fix:** store raw path bytes in the index
(hex/base64/percent-encoded) for non-ASCII members, or carry a `raw_path_b64`
field alongside the lossy display path.

### M3 — `round_up_512` can overflow on a corrupt tar size
`archive_ingest.rs:1592-1594`: `value.div_ceil(512) * 512` — the `* 512` wraps
(release) or panics (debug) for `size` near `u64::MAX`, and `size` comes from
`parse_tar_size`, which accepts base-256 values up to `u64::MAX` from
medium-sourced headers. Inconsistent with the checked arithmetic used elsewhere
in the same file. **Fix:** checked round-up returning a typed error.

### M4 — `--scan-only` does the full work (hash + wrap), not a cheap dry run
`scan_only_report` routes through `materialize_inputs`, which hashes every
native file (`hash_for_manifest`) and **creates every wrapper tar** before
producing the report. The design wants the dry-run cheap and bounded; on a 1 TB
messy tree `--scan-only` would hash 1 TB and materialize all wrappers. It does
correctly avoid the RAO/object write (CLI lane confirmed), so the *output* is a
dry run; the *cost* is not. **Fix:** give `--scan-only` a classification-only
walk (decide + dir-stats + clusters; no hashing, no tar creation, no
`files`/`manifest_entries` population).

### M5 — Scan holds every path in RAM; not bounded-memory for large trees
`PlannerState.files` and `manifest_entries` accumulate one record (with full
path strings) per entry across the whole tree. The clustered *report* is
bounded, but its backing is not. For the build path this is partly inherent
(the RAO writer needs the member list), but combined with M4 it makes both
scan and build O(entries) in RAM. **Fix:** at minimum exclude this from the
M4 scan-only path; consider streaming the member list to the writer for very
large granular builds.

### M6 — "straggler" verdict missing; sanity-ceiling mis-modeled
`scan_report` (`:1689-1709`) produces only `blob-suggest` and `sanity-ceiling`;
the design names three buckets. Worse, sanity-ceiling fires per-directory
(any single dir with `noncompliant ≥ ceiling`), so a legitimately dense blob
target is mislabeled "wrong source", while a genuinely spread-thin tree
produces no verdict. **Fix:** model sanity-ceiling as a whole-scan residual
(noncompliant not absorbed by any chosen blob suggestion) and add the
`straggler` bucket for sparse isolated noncompliant entries, or record the
omission in a conformance backlog.

### M7 — `record_dir_stats` rolls the leaf entry up as a phantom directory
`archive_ingest.rs:1636-1646` bumps a `dir_stats` key for **every** path
component including the leaf file itself, so a leaf becomes a `total=1,
noncompliant=1, ratio=1.0` "directory". At a low operator-tuned `N` this lets a
single noncompliant file self-suggest as a blob — the opposite of the density
intent. **Fix:** roll up only the parent-directory chain (drop the final
component for files/symlinks); include a directory's own name only when it is
itself a directory.

### M8 — `validate_wrap_tar_paths` rehashes the entire wrapper to check paths
`archive_ingest.rs:1325-1336` calls the full `build_wrap_index`, which computes
SHA-256 over every member, purely to validate member paths on extract. A
restore-unwrap of a large blob thus reads the whole blob an extra time. **Fix:**
add a path-only header scan (skip payloads, no hashing) for validation.

### M9 — Customer manifest omits `mtime`
`CustomerManifestEntry` (`:134-141`) carries path/kind/size/sha256/wrapper but
not `mtime`; design §A5 specifies "path, size, sha256, **mtime**". (The prompt
item 5 lists only path/size/sha256 — a prompt-vs-design delta; the design
wins.) **Fix:** add `mtime` to the manifest entry.

### M10 — Spec name `rem restore` vs implemented `rem archive extract`
Items 2/6 specify `rem restore` for unwrap + blob-member restore; the behavior
lives on `rem archive extract`, while `rem archive restore` is the unrelated
legacy-dump command. A user following the design runs the wrong command.
**Fix:** alias or document `archive extract` as the RAO restore entry point and
confirm which string the harness drives.

---

## Low / Nits

- **L1** Unbounded recursion (`process_dir`, `subtree_count_bytes`,
  `collect_hardlink_groups`, `collect_remwrap_files`) — stack overflow on a
  pathologically deep tree (design calls out "deeply nested"). Convert to an
  explicit work-stack or impose a depth ceiling with a clear error.
- **L2** Unchecked `+=` in the rollups/totals (`:1650-1663, 488-489, 605-606,
  1736-1737`) — byte sums over sparse-file apparent sizes can wrap (release)
  or panic (debug); inconsistent with the file's own checked arithmetic. Use
  `saturating_add`/`checked_add`.
- **L3** Cross-tree hardlinks collapse the whole archive: `collect_hardlink_
  roots` returns the common ancestor, which for hardlinks in different
  top-level subtrees is the root `""` → `materialize_root_blob` of the entire
  input (`:525-538`). It is spec-faithful ("force dir at the common ancestor")
  but the spec's edge is sharp; consider capping the climb or wrap-unit=file
  with dedup. Recorded (reason "hardlink"), so not silent — but very surprising.
- **L4** Glob `[...]` supports `[!…]` negation but not POSIX `[^…]`, and an
  unterminated `[` fails to match rather than matching a literal `[`
  (`match_class`, `:1098-1127`). Minor gitignore deviations; document or align.
- **L5** Ruleset matched against `to_string_lossy` of non-UTF-8 paths
  (`relative_match_text`, `:1760`) — `blob`/`exclude` decisions on non-UTF-8
  paths use a lossy form. Acceptable (those paths wrap anyway) but worth a doc
  note.
- **L6** Two full stat passes (`collect_hardlink_roots` + the planner walk) plus
  a `has_xattrs` syscall per directory — extra I/O on large trees. Acceptable
  for ingest; note it.
- **L7** `--blob-entry` without `--blob-member` is a silent no-op
  (lib.rs:1308-1313) — add `requires = "blob_member"`.
- **L8** `--out` is silently ignored under `--scan-only` (lib.rs:1184) — add
  `conflicts_with`.
- **L9** AppleDouble fidelity test validates only the Linux byte-round-trip of a
  `._`-named file; on macOS bsdtar applies AppleDouble merge semantics, which
  is where the real risk is. Note the platform limitation.
- **L10** Shallow assertions: the lint test uses `.any(kind==…)` with no
  line/count check; `--manifest-out` asserts only `format` + one sha256 +
  `exclusions[0].reason` (not ruleset name/hash, sizes, kinds);
  `--scan-only` asserts "no `.rao`" rather than full no-mutation. Tighten.
- **L11** Likely-dead `manifest_kind` (`:1466`) — identity map, verify usage or
  remove.
- **L12** `data_offset = offset + 512` (`:1422`) unchecked — cosmetic vs the
  checked posture elsewhere.

## Untested behaviors (DoD gaps)

- File-fallback wrap (`wrap-unit=file`) through the build pipeline — no test
  drives a granular non-UTF-8/xattr/device single file to a one-member
  `.remwrap.tar` (only whole-dir wrapping is tested directly).
- `--no-index` per-rule — parse + suppression path untested.
- `.idx` rebuild-by-rescan — `build_wrap_index` only exercised inside build.
- Density/ceiling verdicts — `blob_suggestions` never asserted by any test.

## Verified correct (evidence retained in lane outputs)

- Glob matcher is memoized over `(pi, ti)` (`:1036`) → polynomial, no
  catastrophic backtracking; `**/`, `**`, `*`, `?`, classes, and basename-at-any-
  depth semantics all correct against the design examples.
- Extract path validates member paths (no absolute, `..`, `.`, empty, NUL) via
  `validate_tar_member_path` (`:1338`) before invoking tar; member output also
  re-checked in lib.rs (`archive_member_path_parts` + symlink rejection).
- No shell anywhere — `Command::new` + discrete `.arg()` (so H1 is option
  injection, not shell injection).
- `parse_tar_size` base-256 path uses checked_mul/checked_add (`:1491`);
  `hash_tar_range` streams in 64 KiB (no full-file load); `next_tar_header_offset`
  checked.
- Classification completes before any object write — the pre-write gate ordering
  holds (lib.rs:5215-5260).
- Scan clustering is bounded by (dirs × reasons) with a 3-sample cap — never a
  flat per-entry dump.
- `--scan-only` writes no object and does not auto-switch; `--manifest-out`
  requires `--rules`; `--key-file`/`--key-id` require `--encrypt`; blob-member
  ranged extract works keyless (rao-plain) and keyed (rao-aead) with real
  byte-comparing tests.
- Sanitized wrapper names append a full-path SHA-256 prefix + `uniquify` dedup
  → no collisions for distinct non-UTF-8 originals.
- No `unsafe`; the few `expect`/`unreachable!` are genuinely unreachable
  (range-request invariant, u64 suffix space) — except the H2 panic, which is
  an arithmetic slice-inversion, not an `expect`.

## Scope-item coverage (prompt §1–6)

| Item | Status |
| --- | --- |
| 1 Ruleset engine + glob + unreachable lint | Works; lint tests shallow (L10); glob nits (L4) |
| 2 Wrapping (blob/file-fallback/hardlink) | Works; **H1 injection**, file-fallback untested, L3 hardlink edge |
| 3 `.remwrap.idx` | Works; **M2 non-UTF-8 keys**, `--no-index`/rebuild untested |
| 4 Conformance scan | Clustering good; **H5 density rule**, M4/M5 cost/memory, M6 buckets, M7 phantom |
| 5 `--rules`/`--manifest-out` | Works; **M9 mtime missing**, manifest assertions shallow |
| 6 Restore unwrap + blob ranged extract | Works (keyed+keyless proven); M8 rehash, M10 naming |

## Suggested order of work (codex)

1. **H1** (`--` before tar operand) and **H2** (pax length guard) — two small,
   security/robustness fixes with direct tests; do first.
2. **H3 + H4** (CI installs `attr`; xattr detection fails loudly or uses a
   syscall crate; gating test hard-fails on missing tools) — one CI/hygiene
   commit; restores the DoD's actual enforcement.
3. **H5 + M6 + M7** (scan heuristic: topmost-only + don't-climb-past-compliant;
   straggler bucket; whole-scan ceiling; leaf-phantom fix) — one scan commit
   with the AppData/Documents test.
4. **M4 + M5** (cheap classification-only `--scan-only`).
5. **M1/M2/M3/M8/M9/M10** — dialect pinning (spec-side decision first), non-UTF-8
   index keys, checked round-up, path-only validation, manifest mtime, restore
   naming.
6. Lows/nits and the untested-behavior gaps as a test-hardening pass; L3 (cross-
   tree hardlink) is a design-edge note — raise with owner before changing
   behavior.
