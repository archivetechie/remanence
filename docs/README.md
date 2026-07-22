# Remanence documentation

This directory holds the user-facing guides and references. The published
format specifications — and their plain-language companion — live in
[../specs/publication](../specs/publication). The root
[README.md](../README.md) is the project entry point.

## Start here

- [guide-quickstart.md](guide-quickstart.md) — runnable walkthrough from
  build to first tape write.
- [architecture-overview.md](architecture-overview.md) — crate stack,
  write/read data flow, invariants.
- [reference-cli.md](reference-cli.md) — `rem`, `rem-debug`,
  `rem-daemon` command surfaces and exit codes.
- [reference-configuration.md](reference-configuration.md) — config
  file, defaults, environment variables, on-disk state.
- [reference-tape-layout.md](reference-tape-layout.md) — what a written
  cartridge contains.
- [guide-troubleshooting.md](guide-troubleshooting.md) — failure modes
  and their remedies.
- [reference-glossary.md](reference-glossary.md) — project and tape
  vocabulary.
- [reference-extended-attributes.md](reference-extended-attributes.md) —
  what file metadata is preserved, how it is stored, and how it behaves on
  restore (including the standard-`tar` fallback).

## Deeper background

- [why-remanence.md](why-remanence.md) — the project's rationale and the
  bets behind the design.
- [tape-identity-lifecycle-explainer.md](tape-identity-lifecycle-explainer.md)
  — how a cartridge acquires and keeps its identity.
- [pfr-reference.md](pfr-reference.md) — partial-file restore mechanics.
- [reference-extract-stream-protocol.md](reference-extract-stream-protocol.md)
  — the ranged-ciphertext extract-stream contract.
- [cli-design-v0.1.md](cli-design-v0.1.md) — the design record behind the
  `rem` / `rem-debug` split.
- [layer5-roadmap.md](layer5-roadmap.md) — daemon surface, slice by slice.
- [formal-verification-status.md](formal-verification-status.md) — what
  is Lean-proved, and what is deliberately not.

[INDEX.md](INDEX.md) lists every document in this directory with a
one-line purpose. Internal engineering records (design iterations, review
transcripts, dispatch records) are kept outside the repository; git
history preserves everything that ever lived here.
