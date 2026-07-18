# Format specifications

The published, citable specifications live in [publication/](publication/):

- [rao-object-format-1.0.md](publication/rao-object-format-1.0.md) — the
  **RAO (Rem Archive Object) Format Specification, Version 1.0**: the
  archival object container, its manifest, closed-form byte-range
  addressing, and the encrypted envelope.
- [rem-parity-1.0-specification.md](publication/rem-parity-1.0-specification.md)
  — the **REM-PARITY Tape Format Specification, Version 1.0**: on-tape
  layout, sidecar parity, bootstrap blocks, and catalog-less recovery.
- [formats-explained.md](publication/formats-explained.md) — the
  plain-language companion: motivation and design, informative only.
- [remanence-test-vectors.tar](publication/remanence-test-vectors.tar) —
  the pinned test-vector archive; its SHA-256 is printed in both
  specifications.

The specifications are the normative fixed points for the formats:
implementations are validated against these documents, not the reverse.
Earlier internal revisions and review records are preserved in git
history, not in the working tree.
