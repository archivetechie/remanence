# Remanence Documentation

This directory holds design notes, review artifacts, and operator notes for
Remanence. The published format specifications live in [../specs](../specs).
The root [README.md](../README.md) is the project entry point; this file is the
documentation map.

## Current Implementation Status

Remanence is pre-alpha, but the implemented surface is no longer limited to the
lower layers:

- Layers 1, 2, 3a, 3b, 3c, and 4 have working Rust implementations.
- Layer 5 has working daemon/API slices for catalog reads, write sessions,
  whole-object read sessions, operation tracking/cancellation, library
  inspection, basic robotics, Unix-socket transport, and mTLS TCP transport.
- Legacy BRU read support and restore/recovery streaming sinks are present.
- Remaining Layer 5 work includes authorization depth, audit-query RPCs,
  ranged/partial reads, live library-event streaming, and production runbooks.

The detailed Layer 5 slice status is maintained in
[layer5-roadmap.md](layer5-roadmap.md).

## RAO Local Object Files

Local RAO object files use `remanence-library`'s `FileBlockSink` and
`FileBlockSource` adapters. They are fixed-block object byte strings containing
only plaintext `rao-v1` bytes or encrypted `RAO1` bytes. Tape filemarks,
bootstrap rows, and REM-PARITY sidecars remain tape-only framing and are not
part of the portable file object.

## Authoritative References

- [../specs/rao-1.0-specification.md](../specs/rao-1.0-specification.md):
  published RAO Format v1.0.
- [../specs/rem-parity-1.0-specification.md](../specs/rem-parity-1.0-specification.md):
  published Rem Tape Parity (REM-PARITY) Format v1.0.
- [spec-v0.4.md](spec-v0.4.md): consolidated architecture, layer contracts,
  on-tape formats, persistence model, security model, roadmap, and glossary.
- [layer5-roadmap.md](layer5-roadmap.md): current Layer 5 daemon/API status.
- [remanence-testing-plan.md](remanence-testing-plan.md): cross-layer test plan.
- [format-driver-streaming-boundary.md](format-driver-streaming-boundary.md):
  native and legacy body-format driver boundary.
- [cli-design-v0.1.md](cli-design-v0.1.md): `rem` / `rem-debug` split and
  stable output contract.
- [pfr-reference.md](pfr-reference.md): partial-file-restore mechanics.
- [why-remanence.md](why-remanence.md): project positioning and motivation.
- [../proto/layer5.proto](../proto/layer5.proto) and
  [../proto/README.md](../proto/README.md): current Layer 5 protobuf surface.
- [../journal](../journal): dated JSON work journal.

Historical design notes remain on disk because many code comments cite their
section numbers. Treat `spec-v0.4.md` and the current roadmap as authoritative
when a historical design note conflicts with current code.

## Crate Map

```text
remanence-scsi      Layer 1 SCSI CDB and SG_IO primitives
remanence-library   Layer 2 library discovery/ops/watch and Layer 3a tape I/O
remanence-format    Native rao-v1 body format
remanence-bru       Legacy BRU archive reader
remanence-parity    Layer 3c sidecar parity, scan, resume, recovery
remanence-stream    Restore and recovery streaming sinks
remanence-state     Layer 4 catalog, audit log, config, lock protocol
remanence-api       Layer 5 gRPC service implementations
remanence-daemon    rem-daemon service host
remanence-cli       rem and rem-debug command surfaces
remanence-chaos     Fault-injection scaffolding
```

## Build And Test

```sh
cargo build --workspace
cargo test --workspace --exclude remanence-chaos
```

Hardware, VTL, and large-memory tests are ignored by default and require the
environment variables documented in their test modules.
