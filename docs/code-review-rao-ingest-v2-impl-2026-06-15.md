# Code review — RAO ingest-v2 + format completion implementation, 2026-06-15

**Scope:** the implementation of `docs/prompt-rao-ingest-v2-impl.md` — commits
`2c3f7e4` (native hardlinks), `5aeba40` (RAO 1.1 xattrs), `65eb5d9` (ingest v2 +
Phase-3 plugin extraction). Range `c7ec0ec..HEAD`, ~2,850 insertions across
`remanence-format`, the new `remanence-format-driver` crate, `remanence-cli`,
`remanence-stream`, `remanence-api`, and `remanence-bru`.

**Reviewed against:** `specs/rao-1.0-specification.md`, `specs/rao-1.1-specification.md`,
and the four design docs (`rao-hardlinks`, `rao-1.1-metadata-preservation`,
`ingest-layer-implementation`, `foreign-format-plugin-architecture`).

**Method:** claude read the format-layer hardlink/xattr core line-by-line;
three parallel lanes covered Phase 1 (format details/vectors), Phase 2 (ingest
CLI units A–D), Phase 3 (plugin extraction). All findings verified in code.

**Gates:** `cargo fmt --check` clean; `clippy --workspace --all-targets
-D warnings` clean; **1,330 tests pass / 0 fail** (format 64 lib + 14 vectors +
negative suites; cli 99; bru 15; format-driver 1).

## Verdict

**Strong, faithful implementation — no Critical, no High findings.** The hard
parts are all correct: the hardlink model is **option B** (tar-faithful —
zero content fields, resolved via `link_target`, no coord copy), referential
integrity is enforced in *three* places (planner, reader, manifest),
byte-stability is verified concretely (the 1.0 fixtures are byte-identical and
unchanged; `schema_version` bumps to 1.1 **only** when an xattr is present, not
for hardlinks), xattrs are canonical CBOR byte-strings (no base64), the
hostile-input parser is panic-free with checked arithmetic, and the
foreign-format plugin boundary is clean (`cargo tree` confirms core has zero
foreign deps; BRU behind the `foreign-bru` feature; `rem restore` verb shipped;
BRU parser moved verbatim, 15 tests green).

**One Medium**, the rest **Low/Nit** — almost all test-coverage and hygiene.
The Medium is a performance/design-fidelity regression (a reintroduced second
tree-walk), not a correctness bug.

---

## Medium

### M1 — Hardlink ingest reintroduces the second tree-walk the design told us to remove
`crates/remanence-cli/src/archive_ingest.rs:316, 389, 1596-1664`
`collect_hardlink_groups_for_inputs` is a full separate recursive walk run
*before* the main `process_input`/`scan_input` walk, in **both**
`materialize_inputs` and `scan_only_report`. The design (Item 4 / Unit D.3)
explicitly says to remove it: *"Removes the `collect_hardlink_roots` second
tree-walk … inode grouping by `(dev, ino)`, a stat per file the classifier
already does."* This re-stats every file and undercuts exactly the goal Unit B
(cheap `--scan-only`) was built for — "one stat per entry, memory O(#dirs)."
The inline primary-selection (`note_native_hardlink`) already does
first-occurrence detection during the main walk; the pre-walk exists only to
know group membership ahead of time for split-accounting.
**Fix:** detect `(dev, ino)` groups inline in the single classify walk (the
stat is already happening there) and defer blob-boundary split-recording to a
post-pass keyed on what was actually emitted; delete
`collect_hardlink_groups_for_*`. Output is already correct — this is purely the
cost the cheap-scan was meant to avoid.

## Low

### L1 — Shared classifier is two primitives, not one verdict function (+ no scan-vs-build parity test)
`crates/remanence-cli/src/archive_ingest.rs` (scan: 796+813; build: 838+855)
The design (Unit B.1) asked for **one** `classify(entry) → Verdict` that both
build and scan call, "so a dry run provably predicts the build." Instead both
paths independently call `decide(...)` then `native_status(...)`, and
`process_leaf` has an extra hardlink branch (870-889) the scan path lacks — so
scan and build can drift. **Fix:** extract a single `classify` returning the
verdict; both tails consume it. Add a test asserting per-entry scan verdict ==
build verdict.

### L2 — The Unit-B "no content hashing" regression test the work order names is absent
`crates/remanence-cli/src/lib.rs:10429`
`archive_build_rules_scan_only_does_not_require_out` only asserts no `.rao` is
written and `blob_entries == 1`; it does not prove the no-hash/no-wrap path or
verdict parity (the DoD's "inject a hash that would panic if called"). The
cheap-scan guarantee currently rests on code-reading. **Fix:** add the
no-hash + parity test.

### L3 — No reader-side hardlink referential-integrity negative vector
`fixtures/rao/negative-plaintext-reader.json`
The reader/verifier enforcement (`reader.rs:1004` `validate_hardlink_reference`)
is real and reachable, and the *writer* negatives exist
(`hardlink-missing-target` / `-nonregular-target` / `-forward-target`), but no
negative vector feeds a hand-crafted archive with a bad hardlink target through
`read_*`/`stream_*` to confirm `InvalidHardlinkTarget` surfaces on the read
path — which §4.9/§7.4 require. **Fix:** add reader negatives (target absent /
forward / points-to-symlink-or-dir), mirroring the writer trio.

### L4 — No excluded-primary hardlink edge test
The "first non-excluded member becomes primary" behavior is correct by
construction (excluded entries return before the hardlink branch) but untested
(design D.2). **Fix:** add the edge test.

### L5 — Foreign-format CI guard under-covers the "core has zero foreign deps" invariant
`.github/workflows/ci.yml:42-48`
The shipped guard greps `cargo tree -p remanence-cli` only, at default
features. It does not cover `remanence-api` (also has an optional `foreign-bru`
dep) nor the core crates directly. The platform-layer equivalent
(`crates/remanence-library/tests/platform_dependency_guard.rs`) is a
parsed-manifest `#[test]` with an allowlist — stronger and feature-independent.
**Fix:** add a manifest-parse test in that style asserting `remanence-format`,
`remanence-format-driver`, and `remanence-stream` never list a foreign-format
crate; extend the CI grep to `remanence-api`. Today nothing prevents a future
edit from adding a non-optional `remanence-bru` dep to core.

## Nits

- **chunk_count formula duplicated** in `layout.rs:327`, `manifest.rs:156-160`,
  `reader.rs:1375` — all agree, drift-prone; hoist to one helper.
- **`scan_only_report` allocates a tempdir it never writes to**
  (`archive_ingest.rs:384-387`) — harmless dead allocation; drop it.
- **`WrapIndexEntry.path` not renamed to `name`** — the work order said "if
  cheap"; non-normative, no action needed.
- **`remanence-cli` shadow `const BRU_BLOCK_SIZE = 2048`** under
  `#[cfg(not(feature = "foreign-bru"))]` duplicates the canonical constant so
  the dev helper builds plugin-free — acceptable; could re-export instead.
- **Design-doc wording vs shipped:** `foreign-format-plugin` §4 said "remove
  the BRU-specific `archive restore`"; `archive restore` now exists as the
  *generic* `--format` restore (BRU-specific logic gone) — satisfies intent;
  the two design docs were worded inconsistently. No action.

## Verified conformant (evidence retained in lane outputs)

- **Hardlinks (option B):** `RemTarFileSpec::hardlink` carries `size_bytes 0`,
  `file_sha256 None`, `link_target` only; writer emits typeflag `1`, size 0, no
  coord copy (fixture `file_sha256` token count = 2, the two primaries, not 4);
  manifest rejects `file_sha256` on hardlinks; referential integrity enforced
  in planner (`layout.rs:71-85`), reader (`reader.rs:1004-1022`), and manifest
  (`manifest.rs:200-229`) via an incrementally-built `seen_regular_paths`
  (target must be a regular primary appearing before the link); restore via
  `fs::hard_link`, primary-before-link; shared-inode asserted (`nlink == 2`).
- **Byte-stability:** regular-only manifests emit no `entry_type`/`link_target`/
  xattr tokens, empty `metadata_preservation_data`, `schema_version 1.0`; 1.0
  fixtures byte-identical and unchanged in the diff.
- **RAO 1.1 xattrs:** CBOR byte-string values, canonical key order,
  `schema_version 1.1` iff a non-empty xattr container exists; hardlinks
  forbidden from carrying xattrs; restore reapplies via `setxattr`.
- **Unit A escaping:** encoder/decoder implement the three rules exactly,
  bijective; lookups compare raw bytes; collision test (`r\xe9sume` vs
  `r\xeesume`) passes.
- **Unit B:** records-only scan path (no hashing/tar/.idx/file-list); xattr
  detection via the `xattr` syscall crate (no `getfattr`).
- **Unit C:** built-in junk baseline always dropped; `option xattr-mode
  denylist|allowlist` + `xattr-keep/-drop` parsed and validated; fail-safe
  denylist default; small→annotation / oversized(>4 KiB or >16 KiB total)→wrap;
  drops recorded; xattr presence no longer a wrap trigger for regular files.
- **Unit D:** `(dev, ino)` detection; native primary + typeflag-1 link; old
  `collect_hardlink_roots`/common-ancestor/cross-tree-collapse machinery gone;
  `nlink > 1` no longer a wrap trigger; blob-boundary split → independent copy,
  recorded.
- **Phase 3:** `remanence-format-driver` is a clean trait crate (no dep on
  `remanence-format`); `cargo tree` shows core has zero foreign deps; BRU behind
  `foreign-bru` (off by default); generic `--format` dispatch with a clear
  "format not available in this build" error; `rem restore` aliases the native
  extract path; FormatError/FormatGate moved to the driver crate with a
  back-compat `error` shim; no circular deps.
- **Hostile input:** panic-free manifest/pax/tar parsing, checked arithmetic,
  `try_reserve_exact`, bounded decoder; no reachable `unwrap`/`expect`/panic.

## Suggested order of work (codex)

1. **M1** — inline hardlink detection into the single classify walk; remove the
   second tree-walk (restores the cheap-scan cost profile).
2. **L1 + L2** — unify the classifier into one `classify→Verdict` and add the
   scan-vs-build parity + no-hash regression test (one change; closes the
   "dry run provably predicts build" gap and the Unit-B DoD test).
3. **L3 + L4** — add the reader-side hardlink negative vectors and the
   excluded-primary edge test.
4. **L5** — manifest-parse dependency-guard test covering the core crates +
   `remanence-api`.
5. Nits opportunistically (hoist `chunk_count`, drop the scan-only tempdir).

No spec changes required — the implementation conforms to the specs as written;
these are implementation-fidelity (M1) and test-coverage/hygiene items.
