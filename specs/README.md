# Format specifications

The published, citable specifications live in [publication/](publication/):

- [rem-object-core-format.md](publication/rem-object-core-format.md) — the
  **REM-OBJECT Core Format 1.0** specification: the archival object
  container, its manifest, and closed-form byte-range addressing.
- [rem-encrypt-profile.md](publication/rem-encrypt-profile.md) — the
  **REM-ENCRYPT 1.0** specification: the encrypted envelope around a
  canonical REM-OBJECT.
- [rem-parity-1.0-specification.md](publication/rem-parity-1.0-specification.md)
  — the **REM-PARITY Tape Format Specification, Version 1.0**: on-tape
  layout, sidecar parity, bootstrap blocks, and catalog-less recovery.
- [formats-explained.md](publication/formats-explained.md) — the
  plain-language companion: motivation and design, informative only.
- [remanence-test-vectors.tar](publication/remanence-test-vectors.tar) —
  the pinned test-vector archive; its SHA-256 is printed in the
  specifications.

The specifications are the normative fixed points for the formats:
implementations are validated against these documents, not the reverse.
Earlier internal revisions and review records are preserved in git
history, not in the working tree.
