# Public decision log

This log records decisions that affect users, independent implementations, or
the long-term readability of media. Git history remains the detailed engineering
record; this page is the stable public summary.

## 2026-07-31 — Public format review milestone

The project published REM-OBJECT Core, REM-ENCRYPT, and REM-PARITY, the three
format specifications, as review drafts with deterministic test-vector
archives. Their DOIs (persistent citation identifiers) were reserved, and the
records were not yet deposited.

## 2026-08-09 — REM-PARITY terminal-index layout

The implementation changed its parity bootstrap and sidecar schema to
`schema_major` 2 while the public review archive stayed at `schema_major` 1.
The reader rejects `schema_major` 1 bootstrap records. Reading them would need
an explicit migration reader, because the shared REM-PARITY 1 name does not
imply compatibility.

## 2026-09-06 — Compatibility boundary stated

The documentation and the publication test state and assert the
`schema_major` boundary. Publication deposits wait until a self-consistent
review candidate, vectors, and implementation can be deposited together. The
repository crates are marked non-publishable until crate names and a release
policy are set.
