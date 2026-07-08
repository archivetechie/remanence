# Architecture overview

How the pieces of Remanence fit together, grounded in the code as it is
today. For byte formats see the [tape layout reference](reference-tape-layout.md);
for the historical design record see the layer design docs indexed in
[INDEX.md](INDEX.md).

<!-- code-anchor: Cargo.toml @ 7fb10f8 -->
## The layer model

Remanence is organized as a strict stack. Each layer only knows about the
one below it, and the two lowest layers are deliberately format-free so
other tools can build on them:

```text
Layer 5   remanence-api, remanence-daemon, remanence-cli
          gRPC services, drive actors, sessions, operator CLIs
Layer 4   remanence-state
          config, state lock, audit log, rebuildable SQLite catalog
Layer 3   remanence-format (3b: rao-v1 body)   remanence-parity (3c: parity)
          + remanence-aead (RAO1 envelope), remanence-format-driver (traits),
            remanence-bru (legacy reader), remanence-stream (composition)
Layer 2   remanence-library
          discovery, identity, policy-gated handles, robotics, tape I/O
Layer 1   remanence-scsi
          CDB construction, SG_IO transport, response parsing
```

Layers 3b and 3c are siblings, not stacked: both build on the
`BlockSink`/`BlockSource` traits that Layer 2 defines. The format writes
body blocks; the parity layer owns the physical tape layout around them
(bootstraps, filemarks, sidecars).

![Workspace crate map: layer 5 cli, api, daemon over layer 4 state over layer 3 format, aead, parity, format-driver over layer 2 library over layer 1 scsi, with the format-free platform seam between layers 3 and 2](assets/layer-map.svg)

*Fig. 1 — The workspace as a strict stack: each layer depends only on the one below it, and the format-defining crates (amber) sit directly above the format-free platform seam.*

<!-- code-anchor: crates/remanence-scsi/src/lib.rs crates/remanence-library/src/lib.rs @ 7fb10f8 -->
## Layers 1 and 2: the tape platform

`remanence-scsi` is the leaf crate: it builds CDBs, dispatches them
through the Linux SG_IO ioctl, and parses responses (INQUIRY, READ
ELEMENT STATUS, sense data, mode pages, LOG SENSE, positioning). The
parsers are portable pure Rust; only the transport is Linux-gated.

`remanence-library` turns raw devices into a joined model. Discovery
issues READ ELEMENT STATUS to every changer and INQUIRY + VPD page 0x80
to every sg device, then joins them by serial number: "bay X of library L
holds drive S, reachable at /dev/sgN right now". Device paths are treated
as ephemeral; identity is always the serial, revalidated live when a
handle is opened. Handles gate every state-changing operation behind an
access policy (the allowlist) and track a dirty flag when a command's
completion is uncertain. This crate also carries Layer 3a: drive tape I/O
(fixed-block batched read/write, positioning, readiness classification)
and the udev hot-plug watcher.

<!-- code-anchor: crates/remanence-library/tests/platform_dependency_guard.rs .github/workflows/ci.yml @ 7fb10f8 -->
### The platform seam

`remanence-scsi` and `remanence-library` are the reusable tape-platform
crates, and they are format-free by contract: no RAO, parity, catalog, or
daemon knowledge below this seam. The contract is enforced twice in the
tree: a manifest-parsing test asserts that `remanence-scsi` and
`remanence-aead` depend on no internal crate and that `remanence-library`
depends only on `remanence-scsi`; and CI asserts that the default builds
of the CLI and API do not pull the legacy BRU reader (that one guards the
`foreign-bru` feature seam). An external project can build its own layout
and catalog on the platform crates without inheriting Remanence's formats.

<!-- code-anchor: crates/remanence-format/src/lib.rs crates/remanence-parity/src/lib.rs crates/remanence-aead/src/lib.rs crates/remanence-format-driver/src/lib.rs crates/remanence-stream/src/lib.rs crates/remanence-bru/src/lib.rs crates/remanence-crc/src/lib.rs @ 7fb10f8 -->
## Layer 3: formats and parity

Six crates share this layer:

- `remanence-format` implements `rao-v1`, the native body format: one pax
  tar archive per stored object, chunk-aligned, self-describing, with a
  trailing CBOR manifest. It also implements partial-file reads that skip
  directly to a member's blocks.
- `remanence-aead` is the isolated crypto boundary: the `RAO1` encrypted
  envelope (HKDF-SHA-256 key derivation, ChaCha20-Poly1305 STREAM). It
  depends on no other Remanence crate, so the envelope is auditable on
  its own.
- `remanence-parity` owns the physical tape layout: bootstrap blocks,
  filemark discipline, Reed-Solomon sidecar parity, resume, and the
  catalog-less scan that reconstructs a tape's structure from the media
  alone. On clean reads it is transparent; on medium errors it
  reconstructs missing blocks.
- `remanence-format-driver` publishes the driver traits (probe, scan,
  restore, recover) that native and foreign formats implement.
- `remanence-bru` is the one foreign driver so far: a read-only reader
  for legacy BRU/BRU-PE archives, used for migrating old tapes. It is
  feature-gated (`foreign-bru`) and excluded from default builds.
- `remanence-stream` composes 3b and 3c for whole-file workflows:
  prepass local files, stream them through format plus parity, project
  catalog rows, and restore objects back to a directory (including
  sparse recovery of damaged archives).
- `remanence-crc` is the shared CRC-64/XZ used by the parity structures
  and the audit log.

<!-- code-anchor: crates/remanence-state/src/lib.rs crates/remanence-state/src/state.rs @ 7fb10f8 -->
## Layer 4: state

`remanence-state` holds everything the daemon remembers: the validated
operator config, an exclusive state lock (one daemon per state dir), the
append-only audit log, and the SQLite catalog. The catalog is explicitly
a projection — `rebuild_index_from_journals` replays the audit log and
per-tape journals to regenerate it, and opening it read-only refuses to
migrate. Schema version 12 today, tracked in SQLite's `user_version`,
with tables for tapes, pools, tape files, objects/copies/files, catalog
units, sessions, operations, idempotency keys, media-readiness records,
tape-I/O fences, and the drive-stewardship set (drives, events, health
snapshots, clean runs, alarms).

<!-- code-anchor: proto/layer5.proto crates/remanence-api/src/lib.rs crates/remanence-daemon/src/lib.rs @ 7fb10f8 -->
## Layer 5: daemon and API

The gRPC contract (package `remanence.api.v1`, defined in
`proto/layer5.proto`) is still an implementation draft with no
wire-stability promise. `rem-daemon` serves five of its six services
today:

| Service | Surface |
|---|---|
| `Daemon` | health, version, operation lifecycle (get/list/cancel/watch) |
| `LibraryService` | inventory, drive stewardship, alarms, live status, robotics (move/load/unload/import/export); `StreamLibraryEvents` is defined but returns unimplemented |
| `Catalog` | tapes, pools, tape files, object enumeration, catalog units, reconcile |
| `WriteSessionService` | open, client-streamed append, close, abort; `CheckpointSession` returns unimplemented |
| `ReadSessionService` | open, server-streamed object/file/byte-range reads, close |
| `Audit` | defined in the proto; not yet served by the daemon |

Transports: a Unix socket (peer-uid gated to root or the daemon user,
mode 0660) and an optional mTLS TCP listener. Authorization is
default-deny: a client role arrives either from the mTLS certificate (an
explicit `remanence-role=` subject attribute, never a bare CN) or, on the
trusted local socket, as the system role, and every RPC checks the
role-permission matrix before touching state.

Inside `remanence-api`, each mounted drive is a dedicated actor task that
owns its drive handle; sessions, robotics, and reads are messages to that
actor. This serializes hardware access per drive while letting multiple
drives run in parallel.

![Layer 5 topology: orchestrator, rem, and rem-debug reach rem-daemon over gRPC; the daemon runs default-deny authorization and one actor per mounted drive; actors drive the drive and changer, and rem-debug keeps an allowlist-gated direct SCSI path](assets/daemon-topology.svg)

*Fig. 2 — Layer 5 topology: clients reach `rem-daemon` over the unix socket or mTLS TCP, every RPC passes the default-deny role check, and one actor per mounted drive serializes hardware access; `rem-debug` keeps an allowlist-gated direct SCSI path for break-glass work.*

<!-- code-anchor: crates/remanence-api/src/mount.rs crates/remanence-api/src/pool_write.rs crates/remanence-api/src/write_owner.rs crates/remanence-state/src/index.rs @ 7fb10f8 -->
## The write path

What happens when an orchestrator writes an object:

1. `OpenWriteSession` names a tape pool. The daemon selects a tape by the
   pool's policy and watermarks, reserves it, robots it into a free
   drive, and hands the session to that drive's actor.
2. The actor prepares the drive: rewind, read the bootstrap at BOT, and
   verify the tape UUID matches the catalog's expectation. A tape that
   cannot prove its identity is never written.
3. `AppendObject` streams chunks, which the daemon spools to a private
   staging file under a budget, then verifies the caller's declared
   SHA-256 against the received bytes before anything touches tape.
4. The object is laid out as rao-v1 (encrypted first if requested),
   written through the parity sink in fixed-block batches with periodic
   READ POSITION proofs, and terminated with a filemark; parity sidecars
   follow per the scheme.
5. Commit is one SQLite transaction that projects the object, copy, file
   rows and the tape-file bundle after the tape write completed. The
   locator returned to the caller pins the copy to physical media.
6. Failures fail closed: a failed append poisons the session (a retry
   cannot silently write a fresh mid-tape bootstrap), and
   completion-unknown transfer errors record a durable tape-I/O fence
   that blocks further writes — and daemon startup — until an operator
   releases it.

Idempotency: a repeat of the same `(pool, caller_object_id)` with the
same content returns the committed copy instead of writing twice;
different content under a reused id is a conflict.

<!-- code-anchor: crates/remanence-api/src/read_core.rs crates/remanence-api/src/write_owner.rs @ 7fb10f8 -->
## The read path

`OpenReadSession` resolves the object to a tape, mounts it the same way,
and re-verifies the loaded tape's identity before serving. Reads space
directly to the recorded tape file and stream the payload out in bounded
chunks without materializing the object in memory; file- and byte-range
reads skip to the member's chunk-aligned blocks. Parity stays out of the
way unless a medium error forces reconstruction.

<!-- code-anchor: none -->
## Design invariants worth internalizing

- **The tape is the authority.** Every tape self-describes (bootstrap,
  manifest, sidecars); the host catalog is a rebuildable projection.
  Losing the SQLite file loses no data.
- **Identity over enumeration.** Serials and UUIDs everywhere; `/dev/sg7`
  and barcodes are labels, resolved fresh and never trusted as identity.
- **Refuse to guess.** Uncertain completion marks state dirty or writes a
  fence. The failure surface is explicit because the worst archival
  outcome is "looks fine, isn't".
- **Policy lives above.** Remanence decides how bytes get on tape safely,
  not what to archive or when. Retention, scheduling, and workflow belong
  to the orchestrator calling the API.

<!-- code-anchor: crates/remanence-chaos/src/lib.rs @ 7fb10f8 -->
## Testing infrastructure

`remanence-chaos` wraps any SCSI transport with scenario-driven fault
injection (armed from a SQLite state file, double-gated by environment
variables so it can never engage accidentally in production) and provides
a stateful in-memory model of a drive-plus-changer for hermetic tests.
The workspace's unit and integration tests run against captured hardware
fixtures and that model; hardware-touching tests are `#[ignore]`d and
opt-in via environment variables. Formal verification of the parity,
format, and manifest cores lives under `verif/` (Lean/Aeneas), inventoried
in [formal-verification-status.md](formal-verification-status.md).
