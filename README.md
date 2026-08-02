<!-- code-anchor: none -->
# Remanence — Open-Source LTO Tape Archival Software

Self-describing, catalog-rebuildable tape archives with optional 
encryption, erasure-coded media recovery, and partial-file restore. 
Designed for standards-based linear tape and currently validated on LTO.

[![DOI](https://zenodo.org/badge/DOI/10.5281/zenodo.21551570.svg)](https://doi.org/10.5281/zenodo.21551570)

**Website:** [archivetech.org](https://archivetech.org) — overview, the formats explained, and how to verify them.

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

It is developed against a Quadstor virtual tape library and field-tested
on an HPE MSL3040 with LTO-9 drives.

<!-- code-anchor: Cargo.toml crates proto/layer5.proto @ f643f8c2 -->
## Status

Version 1.0.0 — first software release. The format documents are currently
review drafts, open for comment (see each document's Status section).
REM-OBJECT, its REM-ENCRYPT
profile, and REM-PARITY 1.0 are all frozen: specified, implemented, and pinned
by test vectors, so a tape written today reads back unchanged. REM-PARITY's
conformance and freeze criteria (§18) are discharged and its Appendix C items
closed. The daemon and gRPC control surfaces are released but may still
evolve. The project is young — built against one library family and one drive
generation — so restore and verify real material from your own hardware before
relying on it. Working today:

- Layer 1 SCSI primitives and Layer 2 library discovery, identity,
  robotics, and hot-plug watching, with per-library allowlisting.
- Layer 3 end to end: pipelined, staging-ring-backed fixed-block tape
  I/O with position proofs (this is now the only write/read path — the
  earlier non-pipelined mode and its config flag are gone), a
  watermark-gated host-RAM read reservoir with proof-frontier ranged
  reads, the `rem-object-v1` object format, the REM-ENCRYPT 1.0 encrypted representation
  using the fixed `REMO` format-family magic, and Reed-Solomon sidecar
  parity with recovery, resume, and catalog-less scan. The encrypted
  representation's fresh per-object key is wrapped to multiple recipients
  with HPKE Base mode running the X-Wing post-quantum/classical hybrid KEM
  (ML-KEM-768 combined with X25519, per
  `draft-connolly-cfrg-xwing-kem-10`) and stored in the object's own header.
- Layer 4 state: audit log, per-tape journals, and a SQLite catalog that
  is a rebuildable projection, plus media-readiness records and tape-I/O
  fences.
- Layer 5 daemon: catalog queries, pool-targeted idempotent write
  sessions, object/file/byte-range read sessions with an app-restart
  cold-resume contract (tape-identity check + device position proof),
  an advisory per-drive assignment projection for external arbitration,
  operations with cancellation, library inspection and robotics, drive
  stewardship, alarms, live status, over a Unix socket and optional
  mTLS TCP.
- Operator CLIs: `rem` and `rem-debug`, including the destructive-safety
  gauntlet for tape initialization, media-readiness quarantine tooling,
  local REM-OBJECT build/inspect/extract that needs no hardware, and a
  ranged-ciphertext `extract-stream`/`covering-range` pair for bounded,
  memory-cheap partial reads of an object already fetched locally. A
  standalone `rem-recover` binary decrypts archive objects with no
  daemon, catalog, or config file at all — the disaster-recovery path of
  last resort.
- Chaos fault-injection for tests, and Lean/Aeneas proofs over the
  parity and format cores (`verif/`).

The main gaps, from the code as it stands: authorization is a shallow
role matrix, with no scope below the role — no per-pool, per-tape, or
per-object narrowing. Library import/export (mailslot) handling,
library-event streaming, and write-session restart return unimplemented,
as do drive-targeted and tape-targeted write sessions and every
caller-supplied `idempotency_key` (write-session replay detection does
not use that field). Appending to a committed parity tape is reached
only through a write session; the single-object path still refuses it.
Hardware soak coverage is still growing: the format and parity cores are
exercised against a virtual library on every run, but time on physical
LTO-9 iron is episodic rather than continuous.

<!-- code-anchor: Cargo.toml @ f643f8c2 -->
## Build

Rust 1.85+, Linux. No system dependencies for the default build:

```sh
cargo build --release
```

yields `target/release/{rem,rem-debug,rem-daemon,rem-recover}`. `rem`
and `rem-debug` are two binaries built from the same crate (operator vs.
break-glass direct-hardware CLI); `rem-recover` is its own crate with no
dependency on the daemon, catalog, or config file. Optional features:
`remanence-cli/linux-udev` (hot-plug `rem watch`; needs `pkg-config` +
`libudev-dev`). Foreign-format readers are deliberately not included in
the core workspace or binaries; separately versioned distributions can link
adapters through the published registry contract. The auxiliary
[`remanence-adaptors`](https://github.com/archivetechie/remanence-adaptors)
repository is one such distribution, not part of the Remanence core release.

Tests and lints, as CI runs them:

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude remanence-chaos --all-targets -- -D warnings
cargo test --workspace --exclude remanence-chaos
```

Hardware-touching tests are ignored by default and opt in via
environment variables documented in their test modules.

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ f643f8c2 -->
## Quickstart

The native object format works against local files, no tape required:

```sh
rem archive build --inputs some-directory --out demo.rem-object
rem archive inspect --object demo.rem-object
rem archive extract --object demo.rem-object --dest restored
```

`demo.rem-object` is a chunk-aligned POSIX pax tar stream — `bsdtar -tf
demo.rem-object` lists your files — and it is byte-for-byte what a tape write
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
- [Foreign-format adapter reference](docs/reference-foreign-format-adapters.md)
  — the read-only registry boundary and distribution model.
- [Tape layout reference](docs/reference-tape-layout.md) — what is
  physically on a cartridge.
- [Troubleshooting](docs/guide-troubleshooting.md) — failure modes,
  fences, and permissions.
- [Glossary](docs/reference-glossary.md) — project terms and tape
  vocabulary.
- [How the formats change — and what never does](docs/versioning-explained.md)
  — the versioning and revision policy, in plain language.
- [The versioning register](docs/versioning-register.md) — every versioned
  component of the system, one entry each: current values, unknown-value
  behaviour, and how each ever changes.
- [The formats, explained](specs/publication/formats-explained.md) —
  a plain-language companion to the specifications: the motivation and
  the design, without the normative terseness.
- Published format specifications:
  [REM-OBJECT Core Format 1.0](specs/publication/rem-object-core-1-specification.md),
  [REM-ENCRYPT 1.0](specs/publication/rem-encrypt-1-specification.md), and
  [REM-PARITY 1.0](specs/publication/rem-parity-1-specification.md), with
  their pinned test-vector archive alongside.
- [proto/layer5.proto](proto/layer5.proto) — the draft gRPC contract.

<!-- code-anchor: crates/remanence-library/tests/platform_dependency_guard.rs @ f643f8c2 -->
## Migrating foreign tapes

Core Remanence publishes a read-only adapter registry but includes no concrete
foreign-format parser. A separate distribution can link the formats needed for
a particular migration while retaining the same normalized scan, restore,
recovery, and catalog surfaces. See the
[foreign-format adapter reference](docs/reference-foreign-format-adapters.md).

## Platform crate contract

`remanence-scsi` and `remanence-library` are the reusable tape-platform
crates, and they are format-free: no REM-OBJECT, parity, catalog, or daemon
knowledge lives below that seam. `remanence-scsi` depends on no other
Remanence crate, and `remanence-library` depends only on
`remanence-scsi`. A manifest dependency-guard test enforces the
boundary, so external tools can build their own layout and catalog on
the platform crates without pulling in the bundled formats. Portable
REM-OBJECT files follow the same discipline: they contain only the
object's stored bytes — tape filemarks, bootstrap rows, and parity
sidecars are tape-only framing.

<!-- code-anchor: Cargo.toml @ f643f8c2 -->
## Repository layout

```text
crates/remanence-scsi           Layer 1 SCSI CDB/SG_IO primitives
crates/remanence-library        Layer 2 library model/ops and Layer 3a tape I/O
crates/remanence-crc            Shared CRC-64/XZ
crates/remanence-aead           REM-ENCRYPT 1.0 encrypted-representation primitives (fixed REMO family magic; X-Wing HPKE wrapped-DEK)
crates/remanence-format-driver  Published format-driver traits and foreign-adapter registry
crates/remanence-format         Native rem-object-v1 body format
crates/remanence-parity         Layer 3c sidecar parity and recovery
crates/remanence-stream         Restore/recovery streaming composition
crates/remanence-state          Layer 4 catalog, audit, config, lock
crates/remanence-api            Layer 5 gRPC service implementations
crates/remanence-daemon         rem-daemon service host
crates/remanence-cli            rem and rem-debug binaries
crates/rem-recover       Standalone catalogless REM-OBJECT disaster-recovery binary
crates/remanence-chaos          Fault-injection scaffolding (excluded from CI gates)
specs/publication/              Published format specifications + test vectors
docs/                           Guides and references (see docs/README.md)
proto/                          Layer 5 protobuf contract
verif/                          Lean/Aeneas proof targets
fieldtest/                      Physical field-test kit and runbooks
fixtures/                       Captured hardware/SCSI fixtures
fuzz/                           REM-OBJECT fuzz targets
```

![Workspace crate map: layer 5 cli, api, daemon over layer 4 state over layer 3 format, aead, parity, format-driver over layer 2 library over layer 1 scsi, with the format-free platform seam between layers 3 and 2](docs/assets/layer-map.svg)

*Fig. 1 — The crate stack: each layer depends only on the one below it; the highlighted crates define the bytes on tape, and everything below the platform seam is format-free.*

## Contributing and security

Issues and pull requests are welcome — see
[CONTRIBUTING.md](CONTRIBUTING.md) for how the project works day to day.
To report a security issue, especially anything affecting the encryption
envelope or the integrity guarantees, see [SECURITY.md](SECURITY.md);
please do not open a public issue for suspected vulnerabilities.
Release history lives in [CHANGELOG.md](CHANGELOG.md). Released
versions are archived on Zenodo. To cite the software, use its concept DOI
[10.5281/zenodo.21551570](https://doi.org/10.5281/zenodo.21551570) (see
[CITATION.cff](CITATION.cff)). To cite a *format*, use that document's own
concept DOI, named in the document's Status section — the specifications are
deposited separately from the code, so that a citation of the format names an
immutable text rather than a software release.

## License

The Rust reference implementation is `Apache-2.0`, the specifications are
`CC-BY-4.0`, and the conformance vectors are `CC0-1.0`. See
[LICENSING.md](LICENSING.md) for the authoritative path mapping and canonical
license texts.
