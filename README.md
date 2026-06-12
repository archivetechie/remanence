# Remanence

Remanence is open Rust infrastructure for managing physical tape libraries. It
provides a daemon, CLI, local catalog, body-format readers, and tape-parity
layer for LTO library workflows without handing archive policy to the tape
mechanics layer.

The project is pre-alpha. It is actively developed against QuadStor VTL and an
HPE MSL3040 field library; interfaces and on-tape formats are still allowed to
change before a stable release.

## Platform Crate Contract

`remanence-scsi` and `remanence-library` are the reusable tape-platform crates.
They are format-free: no REM-PARITY, RAO, catalog, daemon, or native
object-format knowledge belongs below this seam. `remanence-scsi` must not
depend on any other Remanence crate, and `remanence-library` may depend only on
`remanence-scsi` among internal crates. CI enforces this with a manifest
dependency guard, so external tools can build their own layout and catalog above
these crates without pulling in the bundled filesystem.

## RAO Local Object Files

`FileBlockSink` and `FileBlockSource` in `remanence-library` adapt a local file
as one fixed-block RAO object byte string. The file contains only the object's
stored bytes: plaintext `rao-v1` bytes or the encrypted `RAO1` representation.
Tape filemarks, bootstrap rows, and REM-PARITY sidecars are tape-only framing
and are not embedded in portable RAO object files.

## Current Status

Implemented slices include:

- Layer 1 SCSI core in `remanence-scsi`.
- Layer 2 discovery, robotics operations, and watch scaffolding in
  `remanence-library`.
- Layer 3a drive/tape primitives in `remanence-library::handle::tape_io`.
- RAO encrypted-envelope primitives in `remanence-aead`.
- Layer 3b `rao-v1` body format in `remanence-format`.
- Layer 3c sidecar parity, recovery, resume, and catalog-less scan in
  `remanence-parity`.
- Layer 4 SQLite catalog, audit log, lock protocol, and rebuild paths in
  `remanence-state`.
- Layer 5 daemon/API slices in `remanence-api` and `remanence-daemon`: catalog
  reads, write sessions, whole-object read sessions, operations/cancellation,
  library inspection and basic robotics, Unix-socket transport, and mTLS TCP
  transport.
- CLI surfaces in `remanence-cli`.
- Legacy BRU read support in `remanence-bru` and streaming restore/recovery
  helpers in `remanence-stream`.

Major remaining work includes authorization/RBAC depth, audit-query RPCs,
ranged reads, live library-event streaming, production runbooks, and broader
hardware soak testing.

## Repository Layout

```text
crates/remanence-scsi      Layer 1 SCSI CDB/SG_IO primitives
crates/remanence-library   Layer 2 library model/ops and Layer 3a tape I/O
crates/remanence-aead      RAO encrypted-envelope primitives
crates/remanence-format    Native rao-v1 body format
crates/remanence-bru       Legacy BRU reader
crates/remanence-parity    Layer 3c sidecar parity and recovery
crates/remanence-stream    Restore/recovery streaming sinks
crates/remanence-state     Layer 4 catalog, audit, config, lock state
crates/remanence-api       Layer 5 gRPC service implementations
crates/remanence-daemon    rem-daemon binary/service host
crates/remanence-cli       rem and rem-debug command surfaces
crates/remanence-chaos     Chaos/fault-injection scaffolding
specs/                     Published format specifications
docs/                      Design notes, review notes, roadmap
proto/                     Layer 5 protobuf contract
fixtures/                  Captured hardware/SCSI fixtures
journal/                   Dated JSON work journal
```

## Build And Test

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude remanence-chaos --all-targets -- -D warnings
cargo test --workspace --exclude remanence-chaos
```

Some hardware, VTL, and large-memory tests are ignored by default and require
explicit environment configuration.

## Documentation

Start with [docs/README.md](docs/README.md). The Layer 5 implementation status
is tracked in [docs/layer5-roadmap.md](docs/layer5-roadmap.md), and the current
protobuf surface is [proto/layer5.proto](proto/layer5.proto).

## License

[AGPL-3.0-or-later](LICENSE).
