# Remanence documentation

This directory holds the guides, references, design records, and review
artifacts for Remanence. The published format specifications live in
[../specs](../specs). The root [README.md](../README.md) is the project
entry point; this file is the documentation map, and
[INDEX.md](INDEX.md) is the full registry with per-document status.

<!-- code-anchor: none -->
## Start here

User-facing documentation, kept current against the code:

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

<!-- code-anchor: crates/remanence-api/src/lib.rs crates/remanence-api/src/library.rs @ 7fb10f8 -->
## Current implementation status

Remanence is pre-alpha, but the implemented surface covers the whole
stack:

- Layers 1 through 4 have working Rust implementations: SCSI primitives,
  library discovery/robotics/watching, batched tape I/O, the rao-v1 and
  RAO1 formats, sidecar parity with recovery and scan, and the
  audit/journal/SQLite state layer.
- Layer 5 serves catalog reads, pool-targeted write sessions,
  object/file/byte-range read sessions, operation tracking and
  cancellation, library inspection and robotics, drive stewardship,
  alarms, and live status over Unix-socket and mTLS TCP transports.
- Legacy BRU read support and restore/recovery streaming sinks are
  present (feature-gated).
- Remaining Layer 5 work includes authorization depth, serving the
  audit-query service, live library-event streaming, session
  checkpointing, and parity-tape append.

The detailed Layer 5 slice status is maintained in
[layer5-roadmap.md](layer5-roadmap.md).

<!-- code-anchor: none -->
## RAO local object files

Local RAO object files use `remanence-library`'s `FileBlockSink` and
`FileBlockSource` adapters. They are fixed-block object byte strings
containing only plaintext `rao-v1` bytes or encrypted `RAO1` bytes. Tape
filemarks, bootstrap rows, and parity sidecars remain tape-only framing
and are not part of the portable file object.

<!-- code-anchor: none -->
## Authoritative references

- [../specs/rao-1.0-specification.md](../specs/rao-1.0-specification.md):
  published RAO Format v1.0.
- [../specs/rao-1.1-specification.md](../specs/rao-1.1-specification.md):
  RAO Format v1.1 additive metadata-preservation minor.
- [../specs/rem-parity-1.0-specification.md](../specs/rem-parity-1.0-specification.md):
  published Rem Tape Parity (REM-PARITY) Format v1.0.
- [layer5-roadmap.md](layer5-roadmap.md): current Layer 5 daemon/API status.
- [remanence-testing-plan.md](remanence-testing-plan.md): cross-layer test plan.
- [formal-verification-status.md](formal-verification-status.md) and
  [../verif/STATUS.md](../verif/STATUS.md): proof-target inventory.
- [format-driver-streaming-boundary.md](format-driver-streaming-boundary.md):
  native and legacy body-format driver boundary.
- [cli-design-v0.1.md](cli-design-v0.1.md): `rem` / `rem-debug` split and
  stable output contract (design record).
- [pfr-reference.md](pfr-reference.md): partial-file-restore mechanics.
- [why-remanence.md](why-remanence.md): project positioning and motivation.
- [../proto/layer5.proto](../proto/layer5.proto) and
  [../proto/README.md](../proto/README.md): current Layer 5 protobuf surface.
- [../journal](../journal): dated work journal.

Historical design notes remain on disk because many code comments cite
their section numbers. When a historical design note conflicts with
current code, the code wins; the current user-facing docs above are kept
verified against it.

<!-- code-anchor: Cargo.toml @ 7fb10f8 -->
## Crate map

```text
remanence-scsi           Layer 1 SCSI CDB and SG_IO primitives
remanence-library        Layer 2 discovery/ops/watch and Layer 3a tape I/O
remanence-crc            Shared CRC-64/XZ
remanence-aead           RAO1 encrypted-envelope primitives
remanence-format-driver  Published format-driver traits
remanence-format         Native rao-v1 body format
remanence-bru            Legacy BRU archive reader
remanence-parity         Layer 3c sidecar parity, scan, resume, recovery
remanence-stream         Restore and recovery streaming sinks
remanence-state          Layer 4 catalog, audit log, config, lock protocol
remanence-api            Layer 5 gRPC service implementations
remanence-daemon         rem-daemon service host
remanence-cli            rem and rem-debug command surfaces
remanence-chaos          Fault-injection scaffolding
```

## Build and test

```sh
cargo build --workspace
cargo test --workspace --exclude remanence-chaos
```

Hardware, VTL, and large-memory tests are ignored by default and require
the environment variables documented in their test modules.
