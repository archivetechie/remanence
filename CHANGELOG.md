# Changelog

Notable changes to Remanence and its published formats. The format
specifications carry their own revision histories; entries here are
per-release summaries.

## v1.0.0 — 2026-07-25

First publication release of the Remanence reference implementation and the
open tape-archive format it writes. Archived: concept DOI
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
  The archive SHA-256 is
  `77be73e780e9ff2c265c8357b6ba684b4c69800213820ae1331850f742b1d83d`.
- **Licensing** — the Rust reference implementation is Apache-2.0, the
  specification prose CC-BY-4.0, and the conformance vectors CC0-1.0.
