# Code review — RAO encryption merge (amber absorption), 2026-06-12

**Scope:** everything in `8118512..HEAD` (~13k insertions): the new
`remanence-aead` crate, `remanence-format` RAO integration,
`remanence-parity` bootstrap object rows, `remanence-state` catalog rows,
stream/api/cli plumbing, `fixtures/rao/`, `fuzz/`, and the in-commit spec
amendments. Reviewed against `specs/rao-1.0-specification.md` ("RAO §") and
`specs/rem-parity-1.0-specification.md` ("PAR §") as normative, plus the
Amendment 2 merge map and Rust/security/robustness criteria.

**Reviewers:** claude (crypto core line-by-line + synthesis) with three
parallel review lanes (format integration; parity/state; tests/vectors/fuzz).
Gates at review time: `cargo fmt --check` clean, `clippy -D warnings` clean,
**1,277 tests passing / 0 failing** across 37 suites.

## Verdict

The merge is **high quality at its core**. `remanence-aead` is a faithful,
clean implementation of RAO §5: labels, salt derivation (ctr retry), header
wire layout, metadata schema, nonce construction, full-final-chunks, salt
re-derivation on every keyed open (range opens included), §5.10 keyless
geometry, footer-on-success-only — all byte-exact against the spec. No
unsafe, no panics outside tests, **no RNG dependency at all** (sealing
structurally cannot consume randomness — Amendment 2's biggest risk fully
landed), no internal crate deps. The hostile-input posture is structurally
sound: allocation order means a forged header cannot drive more than the
16 MiB metadata bound before authentication fails.

Test discipline is unusually strong: fixtures byte-exact-verified by the
Rust suite **and** independently re-derived by a Python verifier (different
language, OpenSSL-backed AEAD, hand-rolled HKDF); negative vectors are
single-fault with exact §11 error names and a completeness gate; the
acceptance criteria 4/5/6/7/11 have real byte-comparing tests.
**Critical conformance gate verified: encrypted bootstrap rows cannot carry
manifest anchors — the Rust type makes it unrepresentable, the decoder
rejects both contaminations, and the catalog re-validates at insert.**

**No Critical findings. Five Highs** must be fixed before this can be
called the reference implementation: missing restore-mode digest
verification in the format readers, two defects in the new bootstrap
object-row machinery (unbounded growth; resume row loss), the §8.3 durable
file commit, and the missing §13.1 positive vector suite (freeze blocker).

A process note, accepted as legal: codex amended **both published specs**
in the same commits as the wire changes (PAR §8.2/§8.2.1 key-30 rows;
RAO §4.x symlinks/directories per the approved
`rao-nonregular-entries-design-v0.1.md`). Both specs are pre-freeze drafts
so this is the right mechanism, and the row schema matches the
implementation field-for-field — but the amendments themselves now need a
review pass (see H2/M5/L12), since two halves of the old PAR Appendix C
item 2 (overflow path, leakage analysis) were dropped rather than resolved.

---

## High

### H1 — Format readers never verify per-file digests; restore mode is not the default; salvage mode doesn't exist
`crates/remanence-format/src/reader.rs:119-145, 154-196, 344-453`
RAO §4.9 step 5 makes **restore** (integrity-verifying) the default reader
mode: SHA-256 over delivered payload bytes, compared to
`REMANENCE.file_sha256`, failing the entry with `FileDigestMismatch`
*before* reporting it restored; §5.9 step 9 extends this to the decrypted
inner stream. Today `read_rem_tar_object`, `parse_rem_tar_bytes`,
`stream_rem_tar_object`, and `read_encrypted_rao_object` deliver payloads
unverified; verification exists only in the filesystem restore sink
(`remanence-stream/src/lib.rs:905`) and a **test-only** sink in
`rao_negative_vectors.rs:757-808` — which is the only reason the
flipped-payload-bit vector passes. Any other consumer gets unverified bytes
with success status.
**Fix:** hash payload bytes in the reader loops (reader.rs:313-320, 413)
and fail before `end_file`; add `ReadMode::{Restore, Salvage}` with Restore
the default and salvage explicit and labeled per §4.9; route the inner
stream of `read_encrypted_rao_object` through the same path. Re-point the
negative vector at the public API instead of the test sink.

### H2 — Bootstrap object rows grow unboundedly; no overflow path; tape becomes unfinishable
`crates/remanence-parity/src/sink.rs:1548, 1612`;
`crates/remanence-parity/src/bootstrap.rs:288`; spec gap in PAR §8.2.1/§10.7
Every object appends a row to every subsequent bootstrap payload; nothing
bounds the vector, and rows ride inline even when the sidecar directory
goes external. Once rows alone exceed the block, `checkpoint()`/`finish()`
fail with `BootstrapPayloadTooLarge` — *after* all objects are committed.
Ceiling ≈ **5,900 encrypted / ~3,500 plaintext rows** at 256 KiB blocks ⇒ a
mean-object-size floor of ~3–5 GB on an 18 TB tape. This is precisely the
"inline-versus-external overflow path" half of the old PAR Appendix C
item 2 that the in-commit spec amendment dropped instead of resolving.
**Fix (spec first, then code):** specify the overflow carrier — the natural
home is the parity_map tape file (rows move to a payload key beside the
epoch directory, with the bootstrap carrying the reference) — or, as an
interim, a normative object-count admission check at `begin_object` that
fails the *write*, not the `finish()`. Then implement, with a test at the
boundary (rows exactly filling the block; rows + 1).

### H3 — Resume drops all pre-resume object rows from the authoritative bootstrap
`crates/remanence-parity/src/sink.rs:748-808, 892`;
`crates/remanence-parity/src/journal.rs:1117`
`new_sidecar_only_from_resume` rehydrates the filemark map, directory
entries, boundary, and sequences — but `bootstrap_object_rows` starts
empty. A resumed session's final bootstrap (highest sequence ⇒
authoritative per PAR §8.5) then carries rows only for post-resume objects:
the catalog-less recovery anchors for every earlier object silently vanish.
This violates the root-of-trust completeness principle PAR §14 step 5
establishes for directory entries; journal key 11 already persists the rows
— no constructor consumes them.
**Fix:** carry rows in `ResumeWriterSeed` (sourced from committed journal
entries), load them in `new_sidecar_only_from_resume`, and add the coverage
rule to PAR §8.2.1 ("the final bootstrap's row set MUST cover every object
tape file in its digest scope one-for-one"). Add a resume round-trip test
asserting pre-resume rows appear in the post-resume final bootstrap
(extend `tests/quadstor_parity.rs:972-1000`).

### H4 — `archive build` durable commit violates RAO §8.3 (no fsync before rename, no directory fsync)
`crates/remanence-library/src/block_io.rs:214-219`;
`crates/remanence-cli/src/lib.rs:5112-5180`
§8.3 MUSTs: flush + fsync the file before rename, fsync the containing
directory before reporting success. `FileBlockSink::flush()` is
`File::flush` (a no-op) and exposes no fsync; the CLI renames the temp file
with neither sync. After a crash the final `.rao` path can name unpersisted
data that a catalog already references. (The doc comment on `flush` —
"Flush buffered bytes to the operating system" — is also wrong.)
**Fix:** add `FileBlockSink::sync_all()`; in the CLI: `sync_all()` → 
`fs::rename` → open parent dir and `sync_all()` → only then report/exit 0.
Consider a `commit(final_path)` helper encapsulating the §8.3 protocol so
future file-writing paths can't skip it.

### H5 — §13.1 positive plaintext vector suite missing (blocks freeze criterion 2)
`fixtures/rao/` has only TV-P1/E1/D1 + negative suites. Missing pinned
vectors: empty object; empty file (`chunk_count` 0, `first_chunk_lba`
null); one-byte file; block-boundary set (C−1, C, C+1, multi-chunk);
pathological paths (non-ASCII, >100-byte, 100-byte inline ustar);
full-metadata (mtime, `executable=true`/0755, `executable` null);
multi-file non-alphabetical order; canonical-manifest byte-identity vector.
Partial coverage exists only as unpinned unit tests.
**Fix:** generate each as `fixtures/rao/rao-tv-*.json` (same pattern as
P1), extend `rao_vectors.rs` one test per vector, and extend the Python
verifier and interop gate to cover all of them. Symlink/dir entries now
being in the spec, the suite should also pin: short-target symlink
(linkname), long-target symlink (pax `linkpath`), dangling symlink, empty
directory, and a mixed object — per the design note's §10 vector list.

---

## Medium

### M1 — Reader requires `REMANENCE.encryption`/`REMANENCE.chunk_size` to be present
`reader.rs:785-798`. RAO §4.5.2 items 3–4 are conditional ("**If**
present…"); only `format_id` and `schema_version` may be required.
Conformant streams omitting them are wrongly rejected with `Parse`.
**Fix:** make both gates `if let Some(...)`.

### M2 — `parse_octal` breaks on leading-space-padded octal
`reader.rs:736-749`. Stops at the *first* space even before any digit, so
`"   644 \0"` → 0 and `verify_checksum` falsely fails for space-padded
foreign archives (§4.3.1 requires accepting surrounding whitespace).
**Fix:** skip leading spaces/NULs first, then parse digits, then stop.
Add a foreign-header tolerance test.

### M3 — Manifest per-entry schema validation incomplete
`manifest.rs:116-133`. §4.7.2 consumer obligation 2 requires typed checks
not performed: `executable` true/false/null; `first_chunk_lba` null **iff**
`size_bytes = 0`; `file_sha256` presence rules by entry type;
`entry_type` ∈ {symlink, directory} with `link_target` iff symlink and
neither on regular entries; `chunk_count` equal to the §4.6.4 recomputation.
**Fix:** extend `validate_file_entry` (chunk_size is available for the
recompute).

### M4 — Reader-side pax keyword grammar unenforced
`reader.rs:644-650`. §4.4.1 binds Readers too: keyword non-empty ASCII, no
`=`/newline/NUL → `PaxRecordMalformed`. Empty and non-ASCII keywords are
currently accepted. **Fix:** validate after extracting `key`.

### M5 — PAR §16.4 now contradicts §8.2.1; leakage analysis missing; stale Appendix C cross-reference
`specs/rem-parity-1.0-specification.md` §16.4 still says the format "defines
no per-object descriptive fields" and cites "Appendix C item 2" which now
points at the wrong item after renumbering. The leakage paragraph the old
item promised was never written.
**Fix (docs):** rewrite §16.4: plaintext rows expose manifest
size/chunk-count/digest (acceptable — plaintext objects are non-confidential
by §16.4's own framing); encrypted rows expose `key_id` +
`metadata_frame_len`, both already plaintext in the envelope header on the
same tape (RAO §5.2), so no *new* leakage; fix the cross-reference. The
encrypted-row anchor exclusion (verified, see "Verified" below) is the
load-bearing property — say so.

### M6 — Catalog missing `plaintext_digest`/`stored_digest` (RAO §3.3 is normative); §12.1 belt unimplementable
`remanence-state/src/index.rs:3979-3982`;
`remanence-api/src/pool_write.rs:1294, 1602`. §3.3 places
`plaintext_digest` (all copies) and `stored_digest` (per copy — "the
keyless scrub anchor") in the catalog. The new `object_copies` columns
carry only representation/key_id/metadata_frame_len; the canonical-object
`plaintext_digest` is computed and **discarded** (pool_write.rs:1602);
`objects.content_hash` is the *source-file* hash, not it. Keyless scrub of
encrypted copies has no anchor, and the §12.1 (key_id, hkdf_salt)
insert-time belt cannot be implemented on this schema.
**Fix:** add `plaintext_digest` + `stored_digest` (and `hkdf_salt` if the
belt is wanted — it's SHOULD) to the copy rows; implement the §12.1
repeat-consistency check at insert.

### M7 — Reachable panic in catalog projection for non-regular entries
`remanence-stream/src/lib.rs:498`; `remanence-api/src/pool_write.rs:2027`.
`.expect("catalog projection currently stores regular files only")` —
`file_sha256` is now legitimately `None` for symlink/directory entries, the
involved types and functions are public, and the panic fires *after* tape
blocks and filemark are written, before catalog commit.
**Fix:** typed refusal at plan time, or make the projection's
`file_sha256` optional to match the format model.

### M8 — Bootstrap object-row decoder accepts duplicate CBOR keys
`bootstrap.rs:1001-1140`. PAR §5.3: "Decoders MUST reject duplicate keys."
The new row decoder is last-wins; the journal decoder in the same commit
rejects duplicates — inconsistent rigor on new wire surface.
**Fix:** seen-keys check in the row decoder (and ideally the payload map
loop; note PAR backlog item 11 already tracks the broader ciborium gap).

### M9 — Missing finality negative vector: extra authenticated chunk appended
Only `payload-final-flag-wrong` exists. §13.5 names "a 6th chunk appended /
final chunk re-sealed non-final" — the appended-chunk variant exercises the
footer-offset boundary. **Fix:** add `payload-extra-chunk-appended` via the
existing defective-sealer harness; pin its error.

### M10 — `TrailingData` pinned only on the keyless (advisory) path
Fixture `trailing-byte` uses `inspect`; §13.5 anchors `TrailingData` to
**keyed** open/verify (keyless classification is advisory, §5.10). The
keyed check exists (`open.rs:161`) but no vector pins it.
**Fix:** add an `open`-operation twin of the case.

### M11 — Fuzz plateau corpus not preserved
`fuzz/.gitignore` excludes `/corpus/`; the plateau evidence (153M+ execs,
cov/ft plateaus, zero artifacts) exists only on this machine + journal
prose. A fresh clone cannot replay-confirm criterion 8.
**Fix:** commit the minimized corpora (130/587/486/66/285 files — small) or
archive a tarball with SHA-256 recorded; add a replay (`-runs=0`) step to a
scheduled job.

### M12 — bsdtar interop leg not reproducible or gated
Criterion 3 passed once (journal, /tmp libarchive-tools); bsdtar is absent
on this host and nothing gates it — strict `--check-plaintext-interop`
would fail today. **Fix:** install `libarchive-tools` (journal the sysadmin
step per convention) and wire the strict three-reader gate into CI/harness.

---

## Low

- **L1** `metadata.rs:188-203` — non-integer top-level metadata key →
  `InvalidMetadataField`; RAO §5.5.3 names `InvalidCborEncoding`. Remap.
- **L2** Key-material hygiene: `DerivedKeys` is `Clone` over `Copy` arrays
  (kdf.rs:139-147 zeroizes one stack copy of several);
  `read_root_key_file` (pool_ops.rs:331-341) leaves the 32-byte key in an
  un-zeroized stack array and drops a wrong-length key file un-zeroized.
  Use `Zeroizing<>` end-to-end. (RAO §5.3 SHOULD.)
- **L3** `reader.rs:833-837` — reserved `_remanence/` prefix rejected
  reader-side with `Parse`; §4.6.6 makes rule 3 writer-side, and rejecting
  it breaks the §10 1.x extension posture. Drop or downgrade to a reported
  inconsistency.
- **L4** `reader.rs:198-217` — `InnerObjectMismatch` classification via
  error-message substring matching; names are normative (§11). Return a
  structured gate identifier instead.
- **L5** `manifest.rs:26-29,101-105` + `layout.rs:45-119` —
  `MAX_FILE_ENTRIES` not enforced incrementally in the profile pass and not
  enforced at all by the writer/planner.
- **L6** `reader.rs:307,414` — `REMANENCE.chunk_count` never cross-checked
  against the recomputation (§4.6.2 SHOULD).
- **L7** No API to supply the external bootstrap/catalog `manifest_sha256`
  anchor (§4.7.2 obligation 1 prefers it); readers anchor only to the pax
  self-consistency value. Accept an optional anchor in read/stream entry
  points.
- **L8** Verifier profile (§7.4) unimplemented: manifest-vs-archive
  correspondence, final-fill zero check, report-all-nonconformities. The
  driver honestly advertises `verify: false` — put it on the explicit
  conformance backlog with §4.9 salvage mode (see H1).
- **L9** `tar.rs:104,114` — symlink mode 0o777 / dir mode 0o755 deviate
  from §4.3.1's writer-normative table (0644/0755-iff-executable). Decide
  spec-side (the non-regular amendment should pin these bytes) before
  symlink vectors freeze; cross-implementation byte-identity depends on it.
- **L10** `reader.rs:286-290,401-405` — non-regular entry with nonzero size
  → `InvalidInput`; this is hostile-bytes parsing → should be `Parse`
  (§11.1 maps InvalidInput to caller-supplied metadata).
- **L11** `index.rs:3287` — unknown representation defaults to
  `'plaintext'` on journal replay (an assertion where "unknown" is honest);
  `validate_object_copy_envelope` (index.rs:3371) checks only
  `metadata_frame_len > 0`, not [17, 16 MiB].
- **L12** `pool_write.rs:1295` — catalog stores `manifest_sha256` for
  encrypted objects; RAO §3.3/§7.1 sanction external manifest anchors for
  plaintext copies only. The catalog is a separate trust domain (§12.5), so
  either delete the column value for encrypted rows or add the sanctioning
  sentence to the spec — decide deliberately, spec-first.
- **L13** Key 30 has writers but no readers: the Scanner never validates
  rows against the recovered filemark map (PAR §8.2.1 cross-check is
  writer-side only), and the catalog-less recovery tooling the rows exist
  for doesn't exist yet. Track as backlog with a named owner-milestone.
- **L14** `stream/lib.rs:717-744` — restore symlink creation is
  check-then-create on the full path; §12.10's `openat`/`O_NOFOLLOW`
  component discipline (now load-bearing with symlink entries) is not used.
- **L15** Independent Python verifier not in CI; a pin regeneration that
  keeps Rust tests green would never trigger independent re-derivation.
  Add a CI job gated on `fixtures/rao/**` and format/aead crate changes.
- **L16** No catalog-level test for a *plaintext* build failure leaving no
  durable reference (criterion 7 "a failed build likewise"); clone the
  encrypted transfer-failure test with `Plaintext` representation.
- **L17** `seal-object-id-too-long` vector pins `InvalidObjectIdField`, but
  §11.2 defines that error as empty/interior-NUL/invalid-UTF-8 and maps
  writer-input violations to `InvalidInput`. Spec-side decision: erratum
  §11.2 to include over-length, or remap the vector. Pick deliberately.

## Nits

- `kdf.rs` expand errors → `InvalidRootKey` (unreachable, misleading);
  `stream.rs` `encrypt_*` failures → `AeadAuthenticationFailed`
  (unreachable); `expected_stored_size` is a pure alias of
  `stored_size_from_parts`; `metadata.rs:134` stray `drop(key_encoding)`.
- `pax.rs:74-81` — layout search bound measured from the congruent target,
  not `Rmin` (window up to `chunk_size − 512` wider than §4.6.3's bound;
  determinism corner, practically unreachable).
- `reader.rs:751-761` — `str::parse` accepts a leading `+` in pax
  lengths/sizes; spec grammar is plain decimal.
- `reader.rs:256,368` — ustar `name` UTF-8-validated even when a pax `path`
  override makes it irrelevant (foreign-archive tolerance corner).
- `writer.rs:456` unchecked `blocks_written * chunk_size + buffer.len()`;
  `block_io.rs:515` `VecBlockSink` `.expect` on LBA overflow — both
  unreachable in practice, both inconsistent with the checked-arithmetic
  posture; `reader.rs:163-170` encrypted chunk_size-vs-geometry mismatch
  classified `UnsupportedFeature` (consider `ChunkSizeMismatch` semantics).
- Bootstrap row writer-side validation errors surface as `BootstrapParse`
  though nothing was parsed; siblings use `Invariant`.
- Vector nuances: key_id-swap is a bit-flip (resolver-level swap
  unrepresentable at crate API — document in the fixture); TV-P1 pins
  digests where §13.1 asks for first-block bytes (add `first_block_hex` or
  erratum).

---

## Verified conformant (evidence retained in lane outputs)

- **RAO §5 (the absorbed crypto), in full**: labels; §5.4.1 salt derivation
  + ctr retry + **no RNG anywhere** (no getrandom/rand in the dependency
  tree); §5.2 header bytes/order/validation incl. frozen fields and
  object_id field rules; §5.5 metadata (writer exactly-4-keys; decoder
  enforces shortest-form at every width, encoded-byte key ordering incl.
  nested maps, duplicate rejection, repertoire incl. float/tag/indefinite
  rejection, item/depth caps, exact-payload rule); §5.6 STREAM (nonce
  shape, full final chunks, finality computed never probed, tag-before-
  release); §5.7 footer positional + fill verified + TrailingData; §5.8
  sealing (final-header-before-keys, independent recompute, footer only on
  success — with tests for all three failure paths); §5.9 keyed open in
  order incl. step 4 salt re-derivation **on every keyed open including
  range opens**; §5.10 keyless geometry closed-form; §6 PFR mapping + trim
  rules + only-mapped-chunks-authenticated (proven by damage tests);
  fail-closed wrong-key; deterministic reseal.
- **RAO §4 plaintext representation**: §4.5.2 gates (modulo M1); §4.4 pax
  grammar writer-side + fixed-point pad arithmetic + digit-boundary retry;
  §4.6.3 alignment equation with planner/writer shared sizing; **§4.6.6
  canonical-relative-path enforcement now present writer-side** (the gap I
  flagged pre-merge is closed: `validate_canonical_relative_path`,
  layout.rs:471-497) and reader-side rules 1–2 with `InvalidPath`; §4.7
  manifest profile decoder (encoded-byte sort, depth 8, anchor-before-
  interpretation); §4.8 EOF discipline both sides; §4.9 streaming reader
  with bounded allocations and §12.9-conformant fallible materializing
  path; §5.9 step 9 inner cross-checks present and vector-exercised.
- **PAR-side**: the **encrypted-row anchor exclusion is structurally
  unrepresentable** (enum), decoder-rejected both directions,
  SQL-revalidated at insert; keys 20/21 exclusion intact; §10.7 fit check
  still a real framing attempt with the 4096-byte margin; §11.1 commit
  discipline preserved through the new row path (row captured in the same
  journal-bundle commit; poison semantics intact); checked arithmetic and
  typed errors on all new tape-derived values; schema v6→7 migration
  additive with test.
- **Tests/vectors**: TV-P1/E1/D1 fixtures match every derivable spec number
  (20480/5, pads 1812/2320/1738, manifest 548, M=66, frame at 194, footer
  20754, 24576/6, digests); E1≡P1 digest equality asserted three ways;
  independent Python re-derivation passes for all three; negative suites
  single-fault with exact names + completeness gates; criteria 4/5/6/7/11
  carried by real byte-comparing tests; fuzz targets cover header, both
  CBOR decoders, tar loop, whole-object open with dictionaries and a
  campaign runner.
- **Hygiene**: zero unwrap/expect/panic outside `#[cfg(test)]` in
  remanence-aead and remanence-format (the two M7 expects are in
  stream/api); no `unsafe` anywhere in scope; `RootKey` Debug-redacted +
  zeroize-on-drop; key_id/salt in CLI JSON are public header fields per
  §5.10; clippy/fmt/tests green.

## Freeze-criteria status (RAO §14)

| Criterion | Status |
| --- | --- |
| 1 reference implementation | Open — this report is the gap list |
| 2 fixtures + independent re-derivation | **Blocked by H5**; P1/E1/D1 done incl. Python re-derivation; L15 (CI) |
| 3 tar/bsdtar/tarfile interop | Partial — M12 |
| 4 digest equality end-to-end | **Done** |
| 5 PFR-on-ciphertext by real fetches | **Done** |
| 6 parity-over-ciphertext recovery | **Done** (real ParitySink, keyless repair, fail-closed-before-repair) |
| 7 failed seal/build → no footer/no catalog row | Done (L16 closes the literal wording) |
| 8 fuzz plateau | Done locally; **evidence durability M11** |
| 9 live VTL + MSL3040 two block sizes | Partial — QuadStor leg green (4 KiB + 256 KiB, tar -b); MSL3040 lane exists, not yet run |
| 10 long-term recovery drill | Mechanical drill scripted + green; the independent-party run is a process step for freeze week |
| 11 salt-derivation conformance | **Done** (all four prongs in one test + reader-side negative vector) |

## Suggested order of work (codex)

1. **H1** (reader digest verification + ReadMode) — correctness of every
   restore; do first, it changes reader signatures.
2. **H4** (fsync protocol) — small, isolated, crash-correctness.
3. **H3** (resume row carry) — small; pairs with a quadstor_parity test.
4. **H2** — spec the row-overflow path first (owner/claude review the spec
   sentence), then implement; interim admission check is acceptable to land
   ahead of the full external carrier.
5. **M1–M8** in file order; M5/L12/L17/L9 are spec-side decisions — batch
   them into one spec-errata commit reviewed before code follows.
6. **H5 + M9/M10 + L15/L16** as one fixtures campaign (extend Python
   verifier in the same commit).
7. **M11/M12** evidence durability; journal the sysadmin step for bsdtar.
8. Lows/nits opportunistically; L13/L8 go on the conformance backlog in the
   candidate spec's Appendix B rather than silently aging here.

---

# Round 2 — fix verification (2026-06-12, commits 69c0ede / ac27781 / 080a3c1)

Re-reviewed by claude (Highs and spec edits line-by-line; fixture/CI lane
verified by agent with all suites re-run). Gates: fmt / clippy `-D warnings`
/ full workspace tests all green; `rao_vectors` 12/12;
`rao_negative_vectors` 109 single-fault cases name-asserted; Python
verifier green incl. `--check-plaintext-interop` with bsdtar present.

## Verdicts

| Finding | Verdict | Notes |
| --- | --- | --- |
| H1 reader digest verification | **Fixed** | `ReadMode::Restore` default on every public entry point; digest computed over delivered bytes in all readers incl. the encrypted inner stream; Salvage records mismatches instead of failing; the `FileDigestMismatch` vector now exercises the public reader, not a test sink. Salvage/verifier full profiles deliberately deferred to new RAO App C items 4–5 — acceptable. |
| H2 row growth / overflow | **Partial — one follow-up** | Spec decision is sound (v1.0: no external carrier; normative admission control; coverage rule). Implementation does a real framing-attempt fit check — but at `record_bootstrap_object_row` time, i.e. *after* the object's blocks are written, while the new PAR §8.2.1 sentence requires rejection *"before object bytes are written; failing only at checkpoint or finish() time is nonconformant"*. The check at record time still abandons the boundary safely (no rowless commit, no finish()-time surprise), but it burns a full object write and contradicts the spec's own MUST. Additionally the synthetic fit payload under-models the real final bootstrap: `sequence: 0` (1 CBOR byte vs up to 5), `written_at` empty (matches current writer, OK), and `parity_map_reference: None` while a final bootstrap may carry key 21 (~90–150 bytes) — at the exact block boundary an admitted row set could still fail at finish(). **Follow-up (H2b):** add a `begin_object`-time worst-case admission estimate (row size is computable from representation alone) framed with `sequence = u32::MAX` and a worst-case `parity_map_reference`, keep the record-time exact check as the belt, and widen the synthetic payload the same way. |
| H3 resume row carry | **Fixed** | `ResumeWriterSeed.committed_prefix_object_rows` + validation (`validate_resume_object_rows`: per-row validity, block-size pinning) + loaded into the sink; coverage rule added to PAR §8.2.1; test records a pre-resume row and asserts carry-through. |
| H4 §8.3 durable commit | **Fixed** | `FileBlockSink::sync_all()`; CLI: file sync → rename → parent-dir sync before success. |
| H5 §13.1 vector suite | **Fixed** | All nine fixtures present, arithmetic spec-consistent (independently re-derived: empty=2 blocks, boundary=11 blocks, CBOR null for empty-file `first_chunk_lba`, UTF-8/111-byte/100-byte path encodings, linkname-vs-linkpath split at >100 bytes); byte-exact Rust tests per fixture (full-stream digest + exact manifest CBOR hex + per-entry layout quintuple, bidirectional input assertions); Python verifier + three-reader interop cover all eleven vectors. |
| M1 conditional gates | Fixed | both `if let Some` |
| M2 parse_octal | Fixed | leading-pad skip + test |
| M3 manifest entry schema | Fixed | `first_chunk_lba` null-iff-zero, typed `executable`, `entry_type`/`link_target` exclusivity, chunk_count recompute |
| M4 pax keyword grammar | Fixed | empty/non-ASCII/control/`=` rejected |
| M5 §16.4 leakage rewrite | Fixed | exactly the required analysis; cross-refs corrected; encrypted-row anchor exclusion stated as MUST NOT |
| M6 catalog digests | Fixed | `plaintext_digest`/`stored_digest` columns populated for native copy rows. §12.1 belt remains open (needs a salt column) — tracked below |
| M7 projection panic | Fixed | both expects removed (remaining expects are test-only / static-table) |
| M8 duplicate row keys | Fixed | seen-keys set + test |
| M9 extra-chunk vector | Fixed | `payload-extra-chunk-appended`, keyed open, `AeadAuthenticationFailed` — correct family |
| M10 keyed TrailingData | Fixed | `trailing-byte-keyed` via `open`; keyless twin retained |
| M11 fuzz corpus | Fixed (half) | 1,554 corpus files committed (exact plateau counts); runner does `-runs=0` replay. **No scheduled/CI replay gate** — remaining half below |
| M12 + L15 CI gates | Fixed | CI installs libarchive-tools + cryptography; strict interop gate on every push/PR; local bsdtar install journaled |
| L2/L3/L4/L6/L7/L9/L10/L11/L16/L17 + nits | Fixed | spot-verified: Zeroizing key path, `FormatGate` structured classification, chunk_count cross-check, anchor entry points, unknown-representation projection, plaintext failed-transfer test, §11.2 overlong-object-id erratum, §4.3.1 symlink/dir mode bytes pinned, strict decimal parsing |
| L8/L13/L14 | Deferred properly | now spec-tracked: RAO App C items 4 (verifier/salvage profile) and 5 (O_NOFOLLOW restore hardening); PAR App C item 5 (PAR-KEY30-RECOVERY owner milestone) |

## Remaining follow-ups (small, none blocking daily use)

1. **H2b** — pre-write admission estimate + widened fit model (above). The
   one substantive gap in the round; spec and code currently disagree on a
   MUST that codex itself wrote.
2. **L12 (unaddressed)** — catalog still stores `manifest_sha256` for
   encrypted objects with no RAO §3.3/§7.1 sanction; decide spec-first
   (sanction sentence or NULL the column for encrypted rows).
3. **F5 (now suite-wide)** — all fixtures pin `first_block_sha256` digests
   where §13.1 asks for exact bytes (full stream or first block). Either
   add `first_block_hex` or erratum §13.1 to sanction digest pinning;
   currently a spec/fixture mismatch across the whole suite.
4. **Fixture provenance** — the nine new fixtures were generated by the
   *Python verifier* (`--write-new-plaintext-fixtures`) and confirmed by
   the Rust suite — inverted from §13's "produced by the reference
   implementation … then frozen". Byte equality is proven both ways, so
   the values stand; add a provenance note to `fixtures/rao/README.md`
   (or regenerate from a Rust path) so the independence story stays clean.
5. **M11 second half** — wire the `-runs=0` corpus replay into CI or a
   scheduled job so plateau evidence is continuously confirmed.
6. **§12.1 belt** — needs `hkdf_salt` in copy rows to be implementable;
   SHOULD-level; put on the conformance backlog with M6's schema as the
   landing zone.

**Round-2 verdict:** with H2b and the five small items above, the
implementation will be conformance-clean against both specs as amended.
The fix round was thorough and disciplined: every High except H2's timing
nuance landed exactly as specified, the spec-side decisions were made
deliberately and documented, and deferred items were converted into
spec-tracked open items instead of aging silently.

## Round 2 closure (codex follow-up)

Status after the follow-up commit: all six round-2 residuals above are closed
or explicitly backlogged in the specs.

- **H2b:** `ParitySink` now exposes row-aware object admission. RAO plaintext
  and encrypted callers prove a worst-case key-30 row fits before object bytes
  are written, while `record_bootstrap_object_row` keeps the exact post-write
  check. The fit model now budgets `sequence = u32::MAX`, max-width filemark
  digest counters, and a worst-case key-21 `parity_map_reference`.
- **L12:** encrypted pool writes now store `objects.metadata_hash = NULL`;
  plaintext writes keep the manifest digest anchor.
- **F5 / fixture provenance:** RAO §13.1 now allows `first_block_sha256` for
  large vectors, and `fixtures/rao/README.md` records the Python-first
  generation path for the expanded plaintext suite.
- **M11 second half:** `.github/workflows/ci.yml` has a scheduled/manual
  fuzz-corpus replay job that runs every committed RAO fuzz corpus with
  `-runs=0`.
- **§12.1 belt:** RAO Appendix C now tracks the `hkdf_salt` copy-row field as
  the schema/API prerequisite for enforcing the duplicate-salt catalog belt.
