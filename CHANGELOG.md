# Changelog

Notable changes to Remanence and its published formats. The format
specifications carry their own revision histories; entries here are
per-release summaries.

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
the specifications reached their first published revision on 2026-07-31 and
are deposited separately — see each document's Status section). Archived:
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
