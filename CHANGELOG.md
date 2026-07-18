# Changelog

Notable changes to Remanence and its published formats. The format
specifications carry their own revision histories; entries here are
per-release summaries.

## v1.0.0 — 2026-07-18

First publication release. Archived: DOI
[10.5281/zenodo.21425127](https://doi.org/10.5281/zenodo.21425127)
(concept DOI [10.5281/zenodo.21425126](https://doi.org/10.5281/zenodo.21425126)).

- **RAO Format Specification 1.0** (`specs/publication/`): the archival
  object format — a byte-deterministic, chunk-aligned POSIX pax tar
  container with per-file SHA-256 identities, a CBOR manifest, closed-form
  byte-range addressing, and an encrypted representation sealing each
  object under a fresh key wrapped to multiple recipient public keys
  (HPKE). One encryption scheme; the header's `format_version` byte is
  `2`, with `1` permanently reserved from an unpublished pre-release
  lineage.
- **REM-PARITY Tape Format Specification 1.0**: filemark-delimited object
  layout, Reed–Solomon sidecar parity, self-describing bootstrap blocks,
  and fully specified catalog-less recovery from bare tape.
- **Pinned test vectors** with the archive's SHA-256 printed in both
  specifications, verified by an independent from-scratch Python
  implementation of the read paths.
- **A plain-language companion**, *The Remanence Formats, Explained*
  (`specs/publication/formats-explained.md`).
- Reference implementation: library discovery and robotics, pipelined
  fixed-block tape I/O, the object and parity formats, a rebuildable
  SQLite catalog, a gRPC daemon, operator CLIs, and the standalone
  `rao-recover` disaster-recovery binary.

The implementation itself remains pre-alpha (0.0.x): interfaces may
change; the published on-tape formats are stable as specified.
