# Remanence documentation

This directory holds the user-facing guides and references. The published
format specifications — and their plain-language companion — live in
[../specs/publication](../specs/publication). The root
[README.md](../README.md) is the project entry point.

## Start here

- [guide-quickstart.md](guide-quickstart.md) — runnable walkthrough from
  build to first tape write.
- [status.md](status.md) — what works today, what does not, and how
  mature each part is.
- [architecture-overview.md](architecture-overview.md) — crate stack,
  write/read data flow, invariants.
- [reference-cli.md](reference-cli.md) — `rem`, `rem-debug`,
  `rem-daemon` command surfaces and exit codes.
- [reference-configuration.md](reference-configuration.md) — config
  file, defaults, environment variables, on-disk state.
- [reference-tape-layout.md](reference-tape-layout.md) — what a written
  cartridge contains.
- [guide-object-sizing.md](guide-object-sizing.md) — terminal-index capacity
  terms and the operational tradeoffs for Object and bundle size.
- [guide-troubleshooting.md](guide-troubleshooting.md) — failure modes
  and their remedies.
- [importing-and-recovering-remanence-tapes.md](importing-and-recovering-remanence-tapes.md)
  — safely reconstruct a locally unknown Remanence tape identity before
  inventory and catalog recovery.
- [reference-glossary.md](reference-glossary.md) — project and tape
  vocabulary.
- [reference-extended-attributes.md](reference-extended-attributes.md) —
  what file metadata is preserved, how it is stored, and how it behaves on
  restore (including the standard-`tar` fallback).
- [reference-foreign-format-adapters.md](reference-foreign-format-adapters.md)
  — the read-only compatibility boundary for older archive formats, and
  where adapter crates live relative to core.

## Deeper background

- [why-remanence.md](why-remanence.md) — the project's rationale and the
  bets behind the design.
- [why-sg-passthrough.md](why-sg-passthrough.md) — why the stack
  bypasses the kernel `st` driver and `mt`/`mtx`, told through two
  classic failures of the conventional stack.
- [tape-identity-lifecycle-explainer.md](tape-identity-lifecycle-explainer.md)
  — how a cartridge acquires and keeps its identity.
- [pfr-reference.md](pfr-reference.md) — partial-file restore mechanics.
- [reference-extract-stream-protocol.md](reference-extract-stream-protocol.md)
  — the ranged-ciphertext extract-stream contract.
- [encryption-explained.md](encryption-explained.md) — why REM-ENCRYPT
  seals objects in software instead of relying on LTO drive-level
  encryption, and what an operator still has to manage themselves.
- [rem-implementation-guide.md](rem-implementation-guide.md) — the REM
  Implementation and Operations Guide: informative, tool-neutral practice for
  tools that read or write the REM formats (hostile media, restoring onto a
  host, staging and durability, catalogs, keys and secrets, verification,
  writing, finalizing and reading tapes, capacity admission, ingest, descriptive
  fields). The specifications point to it for everything outside
  conformance.
- [versioning-explained.md](versioning-explained.md) — the three-question
  change policy that governs every format revision, in plain language.
- [versioning-register.md](versioning-register.md) — every versioned
  component (bootstrap, sidecar, object, encryption registries, the
  documents themselves), one entry each: current value, what an unknown
  value means, how it can ever change.
- [formal-verification-status.md](formal-verification-status.md) — what
  is Lean-proved, and what is deliberately not.
- [decision-log.md](decision-log.md) — public format and compatibility
  decisions that affect independent implementations.

This page lists every current document in this directory. Internal
engineering records (design iterations, review transcripts, dispatch
records) are kept outside the repository; git history preserves
everything that ever lived here.
