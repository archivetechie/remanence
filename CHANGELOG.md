# Changelog

Notable changes to Remanence and its published formats. The format
specifications carry their own revision histories; entries here are
per-release summaries.

## Unreleased

- The second implementation of REM-PARITY generation 2 gains a `resume`
  command. It decided the eight portable resume cases under opaque ids, before
  reading the resume expectations or the reference's resumed tapes. All eight
  agree with the pinned expectations. Both accepted resumes reproduce the
  reference's resumed tapes byte for byte, and each equals one uninterrupted
  session. It found seven more places where §14 is silent, including where a
  Resumer learns the parity scheme. CI now re-runs the resume decisions.
- Resume refusals now follow REM-PARITY §14 and §3.4. The resume path reports
  `ResumeAppend` for a prefix that breaks §14's rules, and for commit records
  that are missing, conflict, or are incomplete or ambiguous. It keeps the
  journal's own diagnostic in the message. Before this, some of these cases
  surfaced as journal or state errors. Operational I/O failures keep their own
  errors. A resume refusal now reaches clients as `FailedPrecondition` instead
  of `Internal`, and no client retries it. A reattach at a checkpoint boundary
  now refuses a damaged or unreadable journal before touching the tape, instead
  of carrying on. At shutdown, a write session's checkpoint failure is now
  reported instead of being dropped.
- Added review-only resume vectors for REM-PARITY generation 2
  (`fixtures/rem-parity-terminal-index-draft/tape-images/resume/`). There are
  two accepted resumes, one at W = T and one at W < T. The W < T resume
  rebuilds the open epoch from the tape, and its sidecar equals the one an
  uninterrupted session writes. There are also six refusals and a
  commit-record case. The expected outcomes were written from the text before
  the executor existed. The resumed tapes are pinned by digest.
- Added a second, independent implementation of REM-PARITY generation 2
  (`tools/rem_parity_second_implementation.py`). Its author wrote it from the
  specification text without reading the reference implementation. It
  re-derives every pinned candidate byte from the recorded inputs: the six
  tape images, the six terminal profiles, the maximum artifacts, the
  Object-row extension slots and the million-row stream. All 146 comparisons
  match. It also decided the 25 damage cases before seeing any expected
  outcome, under opaque case ids. 23 of the 24 pinned cases agree. The
  remaining case, a lost filemark that merges an Object with its sidecar,
  turns on a question the text leaves open, and is recorded as such. Its
  findings, including 29 places where the text is silent or ambiguous, are in
  `fixtures/rem-parity-terminal-index-draft/second-implementation/`. CI
  rebuilds everything and re-decides every case.
- Added review-only full-tape candidates for REM-PARITY generation 2
  (`fixtures/rem-parity-terminal-index-draft/tape-images/`). There are six
  tape images: Appendix A.4's minimal tape, a short epoch, two epochs, a
  second edition, and two unfinalized tapes that stop in a torn tail. There
  are 25 damage cases, each of which makes stated records unreadable or
  removes a filemark. One builder regenerates the images from recorded inputs,
  and `MANIFEST.tsv` pins the size and SHA-256 of every tape file; no image
  bytes are checked in. Each case's expected outcome was written from the
  specification text, not taken from the reference. The workspace tests run
  every case through the production Scanner, BOT walk, Recoverer and Verifier.
  24 cases are pinned. The remaining one, whose outcome the text does not yet
  decide, is reported but not asserted.
- The Recoverer can now rescue a parity sidecar whose primary header and
  footer are both unreadable, as REM-PARITY §13.3 step 3 describes, by finding
  its tail copy through the sidecar epoch directory of the tape's final
  ParityMap. Before, the directory was never attached on a generation-2 tape,
  so the rescue could not run. It works both when the inventory comes from a
  terminal replica and after a BOT walk. After a walk, a file is taken to be a
  sidecar only when the reconciled map matches the ParityMap's recorded
  digest.
- A tape whose BOT bootstrap is unreadable can now be read when its tape
  UUID, block size and parity scheme are supplied out of band.
  `rem tape recovery-report` takes `--tape-uuid`, `--block-size` and
  `--scheme` (`k,m,S` or `none`), all three together; the daemon's inventory,
  index verification and reconcile paths use the request's tape UUID and the
  catalog's block size and scheme. A first block counts as unreadable only
  for genuine damage; a readable bootstrap whose checksummed fields disagree
  with the supplied values, or that records drive compression, is refused.
  Results say when the supplied values were used. Scans without these values
  behave as before.
- A Sealer now refuses recipients given out of slot order and a recipient whose
  epoch id is all zero, both with `InvalidInput`, before it draws any key
  material, on the production and the deterministic sealing paths alike.
  Readers are unchanged. The candidate supplement gains the two cases.
- Added a review-only candidate supplement for REM-OBJECT and REM-ENCRYPT
  (`fixtures/rem-object-supplement-draft/`): a Sealer refusing two recipients
  that share an epoch id, an envelope whose metadata hides a negative integer
  under an unknown key, and its valid control. Every case records all of its
  inputs, runs in the Rust harness and in the independent verifier, and is
  pinned by the set's `MANIFEST.tsv`. Its README maps each pinned-archive
  case's section pointer to the section that now describes it. The pinned
  archive is unchanged. The independent verifier now checks the REM-ENCRYPT
  metadata profile everywhere in the metadata, including under unknown keys.
- Recorded the writer's identity consistently in the diagnostic keys. Every
  bootstrap now carries the software (key 3) and the time it was written
  (key 4). The terminal ParityMap carries the software and time at which the
  tape's finalization was planned (keys 6 and 7), and the terminal edition
  names the same software and time. The pair is captured once, before
  planning, and kept in the persisted plan, so re-planning, recovery and media
  adoption reproduce the planned bytes. The finalization companion intent and
  the checkpoint record move to format version 3; version 2 files are refused,
  so a state directory written by an earlier build needs a clean slate.
- Disclosed the REM-PARITY generation boundary: released generation-1 tapes
  and vectors are incompatible with the generation-2 terminal-index reader and
  writer on `main`.
- Corrected the specification publication state: concept DOIs are reserved and
  the first deposits remain pending; advanced the edited review copies to new
  draft identifiers.
- Replaced the vacuous published damage-matrix pass with an explicit,
  generation-checked compatibility rejection test.
- Documented REMP/REMR key-file bytes, transport trust boundaries, and recovery
  streaming semantics; added distinct `rem-recover` failure exit classes and
  `rem-daemon --version`.
- Expanded CI to include the chaos crate, all fuzz corpora, docs with warnings
  denied, MSRV verification, dependency auditing, and a static recovery binary.
- Corrected the declared MSRV from Rust 1.85 to the dependency floor of 1.88.
- Updated `h2` from 0.4.14 to 0.4.16 for RUSTSEC-2026-0258 and `anyhow`
  to 1.0.103 for RUSTSEC-2026-0190.
- Layer 5 wire presence for the terminal inventory, verification and
  finalization messages. 24 fields that some outcomes cannot know are now
  proto3 `optional`, and the daemon leaves them absent instead of sending 0 or
  an empty value: the `TapeInventory` selection, counts and digests;
  the `TapeIndexVerification` verified-prefix counts and digests;
  `TapeIndexSeparationHealth.verified_interior_record_count`; the
  `TapeFinalization` `operation_id`, `completed_replicas` and digests; and
  `TapeInventoryBotObject.stored_block_count`. A real zero, such as a tape with
  no Objects, a finalization before replica A or a two-record separation
  extent, is now sent as a present 0. The other 56 fields in these messages
  stay plain, each with its reason in `tools/wire-presence-ledger.txt`. Field
  numbers and types are unchanged.
- `rem tape inventory`, `rem tape verify-index` and `rem tape finalize` render
  an absent value as JSON `null`. In human output an absent count prints
  `unknown`, and an absent selection or operation id prints `-` as before. Five
  outputs that printed a fabricated `0` now print `null`: structural and
  Object counts for BOT-recovery-required, BOT classification counts on fast
  outcomes, `completed_replicas` for BUSY, `stored_block_count` for a torn BOT
  Object, and `verified_interior_record_count` for INVALID and UNKNOWN
  separations. The CLI accepts a finalization with no `operation_id` when an
  automatic trigger started it, requires both digests on every accepted
  finalization, and rejects a BUSY response that carries any of them. The CLI
  JSON schema identifiers stay at `v1` (and `rem.tape.finalization.v2`); the
  schemas already used `null` for absent values.
- Version skew: an older CLI or client reading a newer daemon is unaffected.
  A newer CLI reading an older daemon sees that daemon's real zeros as
  absent, because an older daemon cannot encode a present zero, and it
  refuses responses where presence is now required (for example an accepted
  finalization without both digests). Run the CLI and the daemon from the
  same build.
- The build no longer needs a system `protoc`. `remanence-api` compiles the
  Layer 5 protos in-process with protox (0.9.1). The generated Rust code is
  byte-identical to the `protoc` output. The descriptor set is identical
  except that protox's built-in well-known types carry no source info, which
  nothing reads. CI no longer installs `protoc`, and it sets `PROTOC` to a
  path that does not exist, so a regression fails the build.
  The README and quickstart now also state the one run-time requirement:
  `bsdtar`, for `rem archive build` and `rem archive extract` when a
  `.remwrap.tar` wrapper is involved.
- `rem archive build` and `rem archive extract` look for `bsdtar` only when a
  `.remwrap.tar` wrapper is involved. A build or scan looks once its plan holds
  a wrapper. A whole-object extract looks before it writes anything when the
  object holds one, so a host without `bsdtar` fails the command before the
  destination changes; it previously restored the files and then exited 1.
  Extract now unwraps only the object's own wrappers and leaves any other
  `.remwrap.tar` already under `--dest` as it is. `--no-unwrap`, range and
  blob-member extracts never need `bsdtar`.
- `tar_engine` is now `null` when no wrapper was involved, in the build
  report's `ingest`, the scan report, the `--manifest-out` manifest and the
  extract report's `unwrap`. A `--map` manifest's `tar_engine` is always
  `null`; it previously named a synthetic `source-map` engine. Under
  `--no-unwrap` the `unwrap` report now carries `tar_engine: null`. A wrapper
  index still always records its engine.
- Every `rem archive extract` JSON report gains `warnings`, an array of reader
  warning names. Whole-object and plaintext `--path`/`--range` extracts report
  `MissingManifest` for an object that reaches tar EOF without its manifest
  (REM-OBJECT §4.9). Encrypted range and blob-member extracts never read the
  manifest, and their array is always empty.
- The REM-ENCRYPT metadata decoder now rejects a negative integer anywhere in
  the value of an unknown key with `InvalidCborEncoding`, as §5.6 requires. It
  previously skipped one. The published vectors are unaffected.

## v0.1.0 — 2026-08-07

**Version renumbered downward: v1.0.0 → v0.1.0. No code changed.**

The July release was numbered 1.0.0. That was wrong. A 1.0 asserts that the
software has been exercised enough, on enough hardware, by enough people, to be
trusted with material that cannot be recovered if it is lost — and this
implementation has not. It has run on one virtual library family and one
physical library with one drive generation, and defects are still being found in
paths that a 1.0 should have shaken out. For software whose entire purpose is
holding the only remaining copy of something, that gap between the number and
the evidence is not a marketing question. It was corrected.

From here the software is **alpha**, and versioned in the 0.x line until the
operational record justifies otherwise.

Three things did **not** change:

- **The format specifications stay at 1.0.** REM-OBJECT Core Format 1.0,
  REM-PARITY 1.0 and REM-ENCRYPT 1.0 are versioned independently of this
  software — see [docs/versioning-explained.md](docs/versioning-explained.md).
  Their 1.0 rests on published text, a pinned test-vector archive, and an
  independent reader written from the specification alone that reproduces every
  vector. That claim is unaffected by the maturity of this implementation. The
  specifications remain review drafts until they freeze on 31 July 2027.
- **The v1.0.0 tag, release and Zenodo deposit remain published**, marked as
  superseded. They are cited by the specifications' provenance sections, and
  withdrawing a version claim in public is the honest way to correct it.
- **The code.** This release is the July tree plus documentation changes.

Note for anyone comparing versions mechanically: 0.1.0 sorts *below* the
withdrawn 1.0.0. That is intentional and is the cost of the correction. Nothing
was ever published to crates.io, so no dependency resolution is affected.

## v1.0.0 — 2026-07-25 (withdrawn; superseded by v0.1.0)

> **Withdrawn 2026-08-07.** This release was renumbered to v0.1.0 because the
> version number overstated the implementation's maturity. The deposit and tag
> remain in place for citation. See the v0.1.0 entry above.

First release of the Remanence reference implementation, distributed with the
format documents as a **review draft** (they were marked "Draft for review";
their concept DOIs were later reserved, but their first separate deposits are
still pending — see each document's Status section). Archived:
concept DOI
[10.5281/zenodo.21551570](https://doi.org/10.5281/zenodo.21551570), version DOI
[10.5281/zenodo.21551571](https://doi.org/10.5281/zenodo.21551571).

- **REM-OBJECT Core Format 1.0** — the durable archival object: a deterministic
  pax tar stream with per-file SHA-256 fixity, a CBOR manifest, and closed-form
  byte-range addressing, so any file or byte range is located by arithmetic with
  no scan, no index, and no decompression.
- **REM-PARITY 1.0** — the on-tape layout: one object per tape file,
  Reed–Solomon parity written in separate sidecar tape files, and repeating
  bootstrap blocks for catalog-free recovery of a bare or damaged cartridge.
- **REM-ENCRYPT 1.0** — an optional, per-object encryption profile: HPKE over the
  X-Wing hybrid KEM (X25519 + ML-KEM-768, per `draft-connolly-cfrg-xwing-kem-10`),
  with the chunk grid preserved so partial restore runs directly on ciphertext.
- **Verification** — a pinned test-vector archive accompanies the specifications;
  an independent Python reader written from the specification text alone
  reproduces every vector, including malformed inputs and their required errors.
  The archive current at this release was
  `b9be8760…`; the archive pinned by the specifications' first published
  revision is
  `77be73e780e9ff2c265c8357b6ba684b4c69800213820ae1331850f742b1d83d`, which
  adds the REM-OBJECT object-row vectors and the independent re-derivation
  tool without altering any pre-existing member.
- **Licensing** — the Rust reference implementation is Apache-2.0, the
  specification prose CC-BY-4.0, and the conformance vectors CC0-1.0.
