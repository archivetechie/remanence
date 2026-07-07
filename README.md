<!-- code-anchor: none -->
# Remanence

Remanence is open Rust infrastructure for writing archives to LTO tape
and getting them back decades later. It is the mechanism layer of an
archive system: a daemon and CLIs that discover tape libraries, move
cartridges, write self-describing objects with erasure-coded parity, and
account for every byte in a rebuildable catalog. What to archive, when,
and for how long are deliberately not its decisions — those belong to
whatever orchestrator calls its API.

The project exists because the long-horizon archive niche is served
mostly by proprietary systems whose on-tape formats die with their
vendors, and by tooling that treats tape like a disk. Remanence takes
the opposite bets: the format on tape is published and readable with
stock `tar`, every tape self-describes so no database is ever the single
copy of the truth, and when the hardware leaves the software uncertain
about physical state, the software stops rather than guesses. The
reasoning is laid out in [docs/why-remanence.md](docs/why-remanence.md).

It is developed against a QuadStor virtual tape library and field-tested
on an HPE MSL3040 with LTO-9 drives.

<!-- code-anchor: Cargo.toml crates @ 7fb10f8 -->
## Status

Pre-alpha, version 0.0.1. Interfaces and the gRPC contract may still
change before a stable release; the published on-tape formats (RAO 1.0/
1.1, REM-PARITY 1.0) are specified and implemented. Working today:

- Layer 1 SCSI primitives and Layer 2 library discovery, identity,
  robotics, and hot-plug watching, with per-library allowlisting.
- Layer 3 end to end: batched fixed-block tape I/O with position proofs,
  the `rao-v1` object format, the `RAO1` encrypted envelope, and
  Reed-Solomon sidecar parity with recovery, resume, and catalog-less
  scan.
- Layer 4 state: audit log, per-tape journals, and a SQLite catalog that
  is a rebuildable projection, plus media-readiness records and tape-I/O
  fences.
- Layer 5 daemon: catalog queries, pool-targeted idempotent write
  sessions, object/file/byte-range read sessions, operations with
  cancellation, library inspection and robotics, drive stewardship,
  alarms, live status, over a Unix socket and optional mTLS TCP.
- Operator CLIs: `rem` and `rem-debug`, including the destructive-safety
  gauntlet for tape initialization, media-readiness quarantine tooling,
  and local RAO object build/inspect/extract that needs no hardware.
- Legacy BRU archive reading (feature-gated) for migrating old tapes,
  chaos fault-injection for tests, and Lean/Aeneas proofs over the
  parity and format cores (`verif/`).

The main gaps, from the code as it stands: authorization is a shallow
role matrix, the audit-query service is defined but not yet served,
library-event streaming and session checkpointing return unimplemented,
parity tapes do not yet support appending further objects after a
committed session, and hardware soak coverage is still growing.

<!-- code-anchor: Cargo.toml @ 7fb10f8 -->
## Build

Rust 1.85+, Linux. No system dependencies for the default build:

```sh
cargo build --release
```

yields `target/release/{rem,rem-debug,rem-daemon}`. Optional features:
`remanence-cli/linux-udev` (hot-plug `rem watch`; needs `pkg-config` +
`libudev-dev`) and `remanence-cli/foreign-bru` (legacy BRU commands).

Tests and lints, as CI runs them:

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude remanence-chaos --all-targets -- -D warnings
cargo test --workspace --exclude remanence-chaos
```

Hardware-touching tests are ignored by default and opt in via
environment variables documented in their test modules.

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 7fb10f8 -->
## Quickstart

The native object format works against local files, no tape required:

```sh
rem archive build --inputs some-directory --out demo.rao
rem archive inspect --object demo.rao
rem archive extract --object demo.rao --dest restored
```

`demo.rao` is a chunk-aligned POSIX pax tar stream — `bsdtar -tf
demo.rao` lists your files — and it is byte-for-byte what a tape write
stores as the object body. The full walkthrough, from local round trip
to library discovery, daemon setup, tape initialization, and a first
tape write, is [docs/guide-quickstart.md](docs/guide-quickstart.md).

## Documentation

- [Quickstart](docs/guide-quickstart.md) — runnable walkthrough.
- [Architecture overview](docs/architecture-overview.md) — the layer
  stack, write/read paths, and design invariants.
- [CLI reference](docs/reference-cli.md) — the `rem`, `rem-debug`, and
  `rem-daemon` surfaces.
- [Configuration reference](docs/reference-configuration.md) — every
  config key, default, and environment variable.
- [Tape layout reference](docs/reference-tape-layout.md) — what is
  physically on a cartridge.
- [Troubleshooting](docs/guide-troubleshooting.md) — failure modes,
  fences, and permissions.
- [Glossary](docs/reference-glossary.md) — project terms and tape
  vocabulary.
- Published format specifications: [RAO 1.0](specs/rao-1.0-specification.md),
  [RAO 1.1](specs/rao-1.1-specification.md),
  [REM-PARITY 1.0](specs/rem-parity-1.0-specification.md).
- [docs/INDEX.md](docs/INDEX.md) — the full documentation registry,
  including design records and reviews.
- [proto/layer5.proto](proto/layer5.proto) — the draft gRPC contract.

<!-- code-anchor: crates/remanence-library/tests/platform_dependency_guard.rs @ 7fb10f8 -->
## Platform crate contract

`remanence-scsi` and `remanence-library` are the reusable tape-platform
crates, and they are format-free: no RAO, parity, catalog, or daemon
knowledge lives below that seam. `remanence-scsi` depends on no other
Remanence crate, and `remanence-library` depends only on
`remanence-scsi`. A manifest dependency-guard test enforces the
boundary, so external tools can build their own layout and catalog on
the platform crates without pulling in the bundled formats. Portable
RAO object files follow the same discipline: they contain only the
object's stored bytes — tape filemarks, bootstrap rows, and parity
sidecars are tape-only framing.

<!-- code-anchor: Cargo.toml @ 7fb10f8 -->
## Repository layout

```text
crates/remanence-scsi           Layer 1 SCSI CDB/SG_IO primitives
crates/remanence-library        Layer 2 library model/ops and Layer 3a tape I/O
crates/remanence-crc            Shared CRC-64/XZ
crates/remanence-aead           RAO1 encrypted-envelope primitives
crates/remanence-format-driver  Published format-driver traits
crates/remanence-format         Native rao-v1 body format
crates/remanence-bru            Legacy BRU reader (feature-gated)
crates/remanence-parity         Layer 3c sidecar parity and recovery
crates/remanence-stream         Restore/recovery streaming composition
crates/remanence-state          Layer 4 catalog, audit, config, lock
crates/remanence-api            Layer 5 gRPC service implementations
crates/remanence-daemon         rem-daemon service host
crates/remanence-cli            rem and rem-debug binaries
crates/remanence-chaos          Fault-injection scaffolding (excluded from CI gates)
specs/                          Published format specifications
docs/                           Guides, references, design records (see INDEX.md)
proto/                          Layer 5 protobuf contract
verif/                          Lean/Aeneas proof targets
fieldtest/                      Physical field-test kit and runbooks
fixtures/                       Captured hardware/SCSI fixtures
fuzz/                           RAO fuzz targets
journal/                        Dated work journal
```

## License

[AGPL-3.0-or-later](LICENSE).
