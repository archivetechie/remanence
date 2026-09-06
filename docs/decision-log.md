# Public decision log

This log records decisions that affect users, independent implementations, or
the long-term readability of media. Git history remains the detailed engineering
record; this page is the stable public summary.

## 2026-07-31 — Public format review milestone

The project marked REM-OBJECT Core, REM-ENCRYPT, and REM-PARITY as their first
public review drafts with deterministic test-vector archives. The format DOIs
were reserved but the records were not yet deposited or published.

## 2026-08-09 — REM-PARITY generation 2 development format

The implementation moved its parity bootstrap and sidecar schema to generation
2 while the public review archive remained generation 1. The generation-2 reader
rejects generation-1 bootstrap records; compatibility requires an explicit
migration reader and is not implied by the shared REM-PARITY 1 name.

## 2026-09-06 — Compatibility boundary made explicit

The documentation and publication test now state and assert the generation
boundary. Publication deposits remain pending until a self-consistent review
candidate, vectors, and implementation can be deposited together. Repository
crates are marked non-publishable until crate names and release policy are
deliberately established.
