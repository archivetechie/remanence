# Remanence — Consolidated Spec v0.4

**Version 0.4 (Consolidated) — May 2026**
**Archives Team**

> This document **consolidates** what the latest code and the most recent layer-specific design docs represent, into one source of truth. It supersedes the documents listed in §16 (Document Lineage). It is normative for current contracts and implementation status as of 2026-06-10.
>
> Where the previous tree had thirteen drifting design documents at multiple versions (`spec-v0.3.md`, `rem-tar-v1-design.md` v0.9.3, `layer3c-design.md` v0.7.2 + the still-around `layer3c-design-v0.7.2.md`/v0.6/v0.5/v0.2 files, `remanence-3c-implementation-addendum-v0.2.md`, `layer4-implementation-addendum-v0.2.md`, `layer3a/3b/2/2b/2c-design.md`, `layer3c-epoch-revision.md`, `3b-catalog-schema-followup.md`), this single doc is the new authoritative spec. The predecessor docs remain on disk as historical artifacts; §16 lists which can be deleted vs. archived.

---

## Contents

- §0 Status and reading order
- §1 Introduction and design priorities
- §2 Operating context
- §3 Architecture (process model, layering, implementation status)
- §4 The Remanence/orchestrator boundary
- §5 Layer 1 — SCSI core (`remanence-scsi`) — **live**
- §6 Layer 2 — Library runtime model (`remanence-library`) — **2a/2b/2c live**
- §7 Layer 3a — Tape mechanism (`remanence-library::handle::tape_io`) — **implemented; MSL3040 smoke pending**
- §8 Layer 3b — Body format (`remanence-format`) — **partially implemented**
- §9 Layer 3c — Tape parity (`remanence-parity`) — **substantially implemented; active hardening**
- §10 Layer 4 — Local state (`remanence-state`) — **partially implemented**
- §11 Layer 5 — gRPC API (`remanence-api`, `remanence-daemon`) — **partially implemented; proto draft at `proto/layer5.proto`**
- §12 Cross-cutting (persistence and authority, hardware abstraction, security, audit, observability, error model)
- §13 Open items per layer
- §14 Roadmap and sequencing
- §15 Glossary
- §16 Document lineage and what this supersedes

---

## 0. Status and reading order

**Reading order for a new contributor:** §1 → §3 → §4 → §12.1 (persistence and authority) → the layer you're working on (§5–§11). Implementation-status banners at the top of each layer section tell you whether a layer is live, partially implemented, or specified-only.

**Implementation status snapshot (2026-06-10):**

| Layer | Crate | Status |
|---|---|---|
| 1 — SCSI core | `remanence-scsi` | **Live.** All SMC-3 + tape-side CDB builders implemented. |
| 2a — Discovery + identity | `remanence-library` | **Live-tested.** QuadStor VTL + production MSL3040. |
| 2b — State-changing ops | `remanence-library` | **Live-tested.** QuadStor + MSL3040. |
| 2c — Hot-plug watcher | `remanence-library::watch` (`linux-udev` feature) | **Implemented; live-tested on akash 2026-05-18.** Daemon-side event streaming remains Layer 5 follow-up work. |
| 3a — Tape mechanism | `remanence-library::handle::tape_io` | **Implemented.** Drive/tape primitives and recovery taxonomy are covered by unit and fixture tests; production smoke testing remains hardware-gated. |
| 3b — Body format / `rem-tar-v1` | `remanence-format` | **Partially implemented.** `rem-tar-v1` planner/writer/streaming reader and the generic format-driver scaffold exist. Registry wiring and legacy foreign-format drivers remain. |
| 3c — Tape parity | `remanence-parity` | **Substantially implemented.** Sidecar parity, recovery, resume, journal, and catalog-less scan are present and actively hardened. |
| 4 — Local state | `remanence-state` | **Partially implemented.** SQLite index, audit, lock, journal, config, rebuild, and pool state surfaces exist. |
| 5 — gRPC API / daemon | `remanence-api`, `remanence-daemon` | **Partially implemented.** Daemon/Catalog, write sessions, whole-object read sessions, operations/cancellation, library inspection/basic robotics, UDS, and mTLS TCP transport exist; authz depth, audit-query RPCs, ranged reads, and live library events remain. |

**Test status:** the default workspace suite is hermetic except for explicitly ignored hardware and large-memory tests. See `docs/README.md` and each test module for current commands and gates.

---

## 1. Introduction and design priorities

Remanence is the **tape-mechanics layer** for LTO-based archives. It exposes a narrow, well-documented interface for higher-level archive management systems to load, write, read, locate, and account for data on tape. It is to a tape library what a filesystem driver is to a block device: a faithful mechanism, not a policy engine.

The name refers to magnetic remanence: the property of a ferromagnetic material to retain its magnetisation after the external magnetising field is removed — the physical reason data persists on tape when power is disconnected, and the reason tape remains the standard medium for long-term archival storage. The command-line interface is named `rem`.

### 1.1 Why this exists

This project was built for a multi-petabyte tape archive. The in-house dwara v2 system is in production but architecturally encumbered; a migration to commercial Atempo Miria has been in progress for an extended period without successful production deployment. Remanence is being developed as a focused, well-engineered alternative to the tape-management portion of the stack — buildable by a small team, defensible on technical merits, easy to integrate with any orchestration layer (the [Sutradhara](https://github.com/archivetechie/sutradhara) project is the planned in-house orchestrator), and deliberately uninterested in the workflows above it.

### 1.2 Design priorities (ordered)

1. **Correctness and data integrity above all.** This system writes the only copies of irreplaceable data.
2. **Minimal scope.** Anything that is not a tape mechanism is out of scope. If three orchestrators ask for the same higher-level feature, that's a signal — not before.
3. **Operational legibility.** An operator with the documentation should be able to understand what the system is doing and why.
4. **Format longevity.** Data written today must be readable in thirty years, ideally with standard Unix tools, even if Remanence itself no longer exists. This is why `rem-tar-v1` is a constrained pax tar subset (§8) and why CBOR formats are canonical and deterministic.
5. **Ease of integration.** Higher-level systems should find Remanence pleasant to integrate with.
6. **Security against realistic threats.** The tool must not be a soft target for attackers seeking to disrupt or damage the archive.

### 1.3 Foundational decisions (from spec-v0.3 §0, reaffirmed)

These six decisions are load-bearing and have not changed:

1. **Minimal mechanism, not orchestration.** Remanence is "filesystem for tape," not an archival system.
2. **OS-portable by construction.** Linux is primary; Windows is a portability goal; macOS is out of scope.
3. **No standalone database.** Authoritative tape state lives on the tape; daemon keeps a regenerable cache + append-only audit log + per-tape journal + rebuildable SQLite query index. PostgreSQL is removed from the architecture.
4. **Pluggable on-tape body format** via registered format drivers with explicit source requirements and capability flags (§8.3). `rem-tar-v1` is the default native body format.
5. **Single-file and byte-range restore (PFR)** are first-class requirements, met by drivers that advertise `indexed_file_restore` and `range_read`.
6. **External API is gRPC with mTLS and bidirectional streaming** (§11). An optional REST gateway can be added later. The `rem` CLI is a thin client over the gRPC interface.

### 1.4 Scope

**Remanence owns:**
- State of every tape library partition the host can reach (drives, slots, mailslot, picker).
- Tape-level operations: load, unload, write, read, locate, seek, eject, import, export, verify.
- Single-file and byte-range restore (PFR) via the on-tape format capability system (§8).
- Tape-level metadata: per-tape state, MAM, health, write history.
- Tape eligibility groups ("tape pools") and each tape's current pool
  assignment, so Sutradhara can request copy-wise, content-wise, or combined
  segregation without Remanence owning placement policy.
- The on-tape catalog format (rem's bookkeeping at file-mark 0 of each tape; not pluggable).
- The on-tape body format **trait surface**, plus a small set of built-in implementations. New formats are added by external crates implementing the trait.
- Hardware health, diagnostics, drive cleaning workflows.
- Optional encryption for tape contents, including key-management integration.
- Reed-Solomon tape parity (Layer 3c), including bootstraps, sidecars, journal, and recovery.

**Remanence does not own:**
- The logical file catalog across all storage tiers.
- Multi-tier orchestration (disk, cloud, tape coordination).
- User-facing archive workflows (ingest, retrieval policy, retention).
- Disk or cloud storage backends.
- End-user authentication (above the API authn layer).
- Cross-tape search ("which tapes contain X?"). Remanence exposes per-tape catalogs; the orchestrator builds search indexes.
- Pool-selection policy ("which copy/content should use which pool"). Sutradhara
  maps copy/content intent to a `pool_id`; Remanence enforces and reports the
  local tape assignment.
- Job scheduling, retention policy, deduplication.
- WORM enforcement beyond what the hardware itself provides.

These are the responsibility of the orchestration layer (Sutradhara, or any equivalent).

---

## 2. Operating context

### 2.1 Hardware

| Library | Drive generations | Notes |
|---|---|---|
| **HPE StoreEver MSL3040** | LTO-9 (primary), LTO-7 | Single chassis, expandable to seven stacked modules / 280 slots. Routinely partitioned into multiple logical libraries — each partition appears as an independent SCSI medium changer (§12.2.4). |
| **Overland Storage XL80** | LTO-7, LTO-6 | Legacy library kept in service. Same SCSI command set as MSL3040 modulo vendor quirks. |

Supported LTO generations across the fleet: **LTO-6 through LTO-9.** Drive count and slot count are configurable per deployment. A single Remanence daemon scales to a fully populated multi-chassis configuration and to a host with multiple libraries attached simultaneously.

### 2.2 Host environment and OS portability

**Primary supported OS: Linux** (Ubuntu 24.04 LTS reference; RHEL 9, Rocky 9, AlmaLinux 9 with kernel ≥ 5.10). The daemon runs as a long-lived systemd service with appropriate Linux capabilities for SCSI generic device access. CentOS 7 and other end-of-life distributions are not supported (LTO-9 first-load calibration interacts poorly with older kernel SCSI driver behavior).

**OS portability is a design constraint.** OS-specific code is confined to named seams behind traits with at least one Linux implementation in-tree:

| Seam | Linux | Other OS |
|---|---|---|
| SCSI passthrough | `LinuxSgTransport` (SG_IO ioctl) | Windows: SPTI via `DeviceIoControl(IOCTL_SCSI_PASS_THROUGH_DIRECT)`. FreeBSD: CAM. |
| Device enumeration | Linux sysfs walker | Windows: `SetupDi*`. Trait: `DeviceEnumerator`. |
| Device path type | `DevicePath(OsString)` newtype | Linux: `/dev/sg0`, `/dev/nst0`. Windows: `\\.\Tape0`, `\\.\Changer0`. |
| Privilege model | tape group + `CAP_SYS_RAWIO` | Windows: admin / `SeManageVolumePrivilege`. Surfaced via `PrivilegeAdvice` trait. |
| Hot-plug notification | udev | Windows: `RegisterDeviceNotification`. |

**Windows** is a portability goal — not a v1 deliverable, but architectural seams must remain clean. **macOS is out of scope** (no SAS HBA ecosystem).

The daemon does not require a container or VM; hardware-managing daemons benefit from direct access to kernel SCSI and hot-plug subsystems.

### 2.3 Integration target

The primary integration target is [Sutradhara](https://github.com/archivetechie/sutradhara), the in-development orchestrator that owns the cross-tier catalog. Remanence exposes its functionality via a gRPC service over mTLS (§11); Sutradhara is a client of that service. The service is usable by any client.

A reference Python client library (generated from the `.proto`) is provided; the `rem` CLI is a Rust reference client.

### 2.4 Coexistence with other tape-handling software

Remanence will routinely operate on hosts where other software *also* talks to the same SCSI hardware — either a different Remanence-managed partition, or a different tape-management tool entirely (e.g. dwara v2 managing the LTO-7 partition of the production MSL3040 chassis). Discovery returns a complete view of every reachable library and partition; operations are gated on explicit per-partition opt-in (§12.2.4 and §12.3.4). **Remanence never operates on a partition it has not been told it owns.**

---

## 3. Architecture

### 3.1 Process model

Remanence runs as a single long-lived daemon process on a dedicated host. **A single daemon manages every logical library the host can reach and the operator has configured it to own.** A logical library is the operational unit: one SCSI medium changer with its own drives, slots, and import/export ports. Whether two logical libraries happen to share a physical chassis is irrelevant — there are no SCSI operations that cross logical-library boundaries, and the daemon treats each one independently.

The daemon exposes a gRPC service for clients and persists a small amount of local state to disk (§3.3). **There is no database server in the architecture.**

The daemon is built in Rust. Drivers: memory safety in code that constructs and parses binary SCSI structures; clean integration with Linux kernel subsystems (sg, udev) via mature crates; suitability for long-running concurrent operations via tokio; single static-binary deployment; trait-based extension points for external format crates.

### 3.2 Layering

```
┌─────────────────────────────────────────────────────────────┐
│ Layer 5 — gRPC API / daemon         remanence-api/daemon    │
│ mTLS/UDS server; session and robotics orchestration         │
├─────────────────────────────────────────────────────────────┤
│ Layer 4 — Local state               remanence-state         │
│ Config, audit log, SQLite query index, idempotency           │
├─────────────────────────────────────────────────────────────┤
│ Layer 3b — Body format              remanence-format/bru    │
│ Format drivers + rem-tar-v1 default + legacy readers        │
├─────────────────────────────────────────────────────────────┤
│ Layer 3c — Tape parity              remanence-parity         │
│ RS GF(2⁸), bootstraps, sidecars, journal, recovery           │
├─────────────────────────────────────────────────────────────┤
│ Layer 3a — Tape mechanism           remanence-library::handle::tape_io │
│ LOAD/UNLOAD/LOCATE/SPACE/READ/WRITE/WRITE_FILEMARKS           │
├─────────────────────────────────────────────────────────────┤
│ Layer 2c — Hot-plug watcher         remanence-library::watch │
│ udev (Linux), event coalescing                               │
├─────────────────────────────────────────────────────────────┤
│ Layer 2b — State-changing ops       remanence-library        │
│ Move, load, unload, import, export; dirty-state machine      │
├─────────────────────────────────────────────────────────────┤
│ Layer 2a — Discovery + identity     remanence-library        │
│ Cold enum, join-by-serial, value types                       │
├─────────────────────────────────────────────────────────────┤
│ Layer 1 — SCSI core                 remanence-scsi           │
│ CDB construction, sg_io, response parsing                    │
└─────────────────────────────────────────────────────────────┘
```

Each layer is testable independently with captured response fixtures. Layer detail follows in §5 through §11.

### 3.3 Persistence

**There is no database.** The authoritative record of what is on a tape is the tape itself. Every cartridge contains:

1. **Bootstrap blocks** (Layer 3c, §9.3): the catalog of tape files, scheme, and filemark-map digest, written at BOT and at content-driven points throughout the tape.
2. **Object archives** (Layer 3b): the actual archived data in the chosen body format. For `rem-tar-v1`, each object is a clean pax tar archive in its own tape file.
3. **Per-object self-description**: rem-tar-v1 objects carry a `manifest.cbor` at the end of each archive with per-file SHA-256 hashes and chunk indexes (§8.7).
4. **Parity sidecars** (Layer 3c): Reed-Solomon parity epochs as separate tape files between objects, allowing recovery of contiguous damage up to ~512 MiB at default scheme.

The daemon maintains regenerable local caches:

| State | Storage | Authority | Loss behavior |
|---|---|---|---|
| Operator config | `/etc/rem/config.toml` | Authoritative (operator intent) | Daemon cannot safely start without valid config |
| Audit log | `/var/lib/rem/audit/YYYY-MM-DD.remaudit` (append-only, fsync'd, hash-chained, daily rotation) | Authoritative — daemon-local actions, idempotency history | Tamper detectable; periodic offsite checkpoints |
| Per-tape 3c journal | `/var/lib/rem/journals/<tape_uuid>.remjournal` (append-only file per tape, CBOR + CRC-64) | Authoritative — committed tape-file resume/commit record | Rebuildable from tape (forward-validation, slower) |
| SQLite query index | `/var/lib/rem/index/rem-state.sqlite` (WAL mode) | Rebuildable projection over journals + audit | Delete and rebuild |
| Per-tape catalog cache | `/var/lib/rem/cache/tapes/<tape_uuid>.cat` | Rebuildable from tape | Re-read on next mount |
| Library inventory snapshot | In-memory; derived from READ ELEMENT STATUS at startup | Tape library hardware is authoritative | Re-derive on each refresh |
| Drive position + loaded tape | In-memory; queried on demand | Drive is authoritative | — |

The audit log is the only genuinely durable daemon-owned state. The 3c journal is durable but rebuildable from tape. The SQLite index and catalog cache are always rebuildable. **Losing all daemon-local state is not a data-loss event** — a fresh daemon mounts each tape and rebuilds everything from the tape's own self-description.

Concurrent access: the daemon is the single writer. Multiple clients call into the daemon over gRPC; the daemon serializes cache, audit, and journal writes internally.

---

## 4. The Remanence / orchestrator boundary

### 4.1 What "tape subsystem" means

**Remanence answers tape questions; the orchestration layer answers archive questions.**

| Remanence answers | Orchestrator answers |
|---|---|
| What is on tape `L91234L9`? | Where are all copies of `clip_001.mxf`? |
| At what LBA does object `obj-0042` start? | Which files have insufficient redundancy? |
| Read bytes `[10 MB, 11 MB)` of object `obj-0042`. | Restore this file from the fastest available source. |
| Which tapes are in the library? | What is our total archive size? |
| What is the health of drive bay 3? | Which files should migrate from tape to disk? |
| Write this byte stream to tape, return location. | Ensure this file has the policy-required copies. |

### 4.2 The caller-object-id pattern

When the orchestrator asks Remanence to write data, it supplies a **caller object identifier** (an opaque string) and optional **caller metadata** (an opaque structured document). Remanence stores both as part of its record of that object on tape, and writes them into the on-tape format alongside the data. Remanence does not interpret the caller object id; the orchestrator uses it to tie tape-resident objects back to its own catalog of files.

This is the integration seam. The orchestrator knows what its identifiers mean and how to map them to logical files. Remanence preserves them faithfully and surfaces them in every catalog query.

### 4.3 Reconciliation

The orchestrator periodically reconciles its catalog against Remanence: it queries Remanence for the contents of each tape and confirms its own records agree. Discrepancies surface bugs or out-of-band events (operator intervention, hardware failure). Remanence exposes the queries needed for this reconciliation as part of its standard API (§11.4 `Catalog.EnumerateObjects` with `reconcile_from_tape=true`); reconciliation is not a special-case workflow.

### 4.4 Identity boundaries

| Entity | Remanence-side identity | Orchestrator-side identity |
|---|---|---|
| Logical asset (file the user cares about) | — | Orchestrator's own ID (e.g. content hash for content-addressed orchestrators like Sutradhara) |
| Object on tape (one rem-tar-v1 archive) | `object_id` (Remanence UUID) + `caller_object_id` (opaque string from orchestrator) | The orchestrator's notion of "this asset has a copy on tape X" |
| File within an object | `(object_id, FileId)` — FileId is format-defined opaque bytes | The orchestrator's path/name/identity for that file |
| Cartridge | `tape_uuid` (Remanence) + `voltag` (operator-visible label) | — |
| Drive bay | `(library_serial, element_address)` | — |
| OS device node | **not stable** — runtime mapping rebuilt by §12.2 join-by-serial | — |

---

## 5. Layer 1 — SCSI core

**Status: live.** Crate: `crates/remanence-scsi`. No business logic; no I/O policy; no concurrency. Testable in isolation against captured response fixtures.

### 5.1 Scope

Pure SCSI command construction (CDB builders), ioctl invocation, and response parsing. Implements every CDB Remanence uses:

**SMC-3 side (library/changer):**
- `INQUIRY` standard + VPD pages (0x00, 0x80, 0x83)
- `READ ELEMENT STATUS` with DVCID and CurData
- `MOVE MEDIUM`
- `INITIALIZE ELEMENT STATUS`
- `PREVENT / ALLOW MEDIUM REMOVAL`
- `LOG SENSE` (any page)
- `READ ATTRIBUTE` / `WRITE ATTRIBUTE` (MAM)
- `SECURITY PROTOCOL IN` / `SECURITY PROTOCOL OUT` (encryption key wrap)

**Tape-side (drive):**
- `LOAD` / `UNLOAD`
- `REWIND`
- `LOCATE(16)` (LBA-based)
- `READ POSITION` (long form, service action 6)
- `SPACE(6)` and `SPACE(16)`
- `WRITE FILEMARKS` (synchronous; IMMED never set)
- `READ` and `WRITE` (variable and fixed block)
- `READ BLOCK LIMITS`
- `MODE SENSE(6)` and `MODE SELECT(6)` (pages 0x0F data-compression, 0x10 device-configuration)

### 5.2 Transport

`Transport` trait with `LinuxSgTransport` as the production impl. The `execute_in` and `execute_out` methods return `Result<TransferOutcome, ScsiError>` where `TransferOutcome` carries SG_IO `resid`, sense buffer, and `TimeoutClass`. A `FixtureTransport` mock backs unit tests.

### 5.3 Fixtures

Captured at `/home/user/remanence/fixtures/`:
- `inquiry/` — standard INQUIRY responses (drives + changers, QuadStor + real-hardware variants)
- `vpd-80/` — VPD page 0x80 (unit serial)
- `element-status/` — READ ELEMENT STATUS (with and without DVCID)
- `real-hardware/<timestamp>/` — full datamover snapshots (lsscsi, dmesg, inquiry tree, vpd-00, vpd-80) from 2026-05-16
- `log-sense/` — *currently empty*; next MSL3040 window should capture sg_logs -a, page 0x17 (volume statistics), page 0x2E (tape alert)

Missing from the fixture corpus: VPD page 0x83 (device identifiers) and the log-sense pages above. Priority for the next access window.

---

## 6. Layer 2 — Library runtime model

**Status: 2a + 2b + 2c live.** Crate: `crates/remanence-library`. The in-memory model of library topology, drive states, and tape inventory. Performs the join-by-serial logic mapping library-reported drive bays to host-side device nodes (§12.2).

### 6.1 Layer 2a — Discovery and identity

**Status: live-tested on QuadStor + production MSL3040.**

Cold enumeration of the SCSI fabric: discover libraries, drives, slots, import/export ports. Value types:

```rust
pub struct Library { pub serial: String, pub vendor: String, pub product: String, pub product_revision: String, /* ... */ }
pub struct Drive { pub element_address: u16, pub serial: String, pub device_path: DevicePath, /* ... */ }
pub struct Slot { pub element_address: u16, pub voltag: Option<String>, pub tape_uuid: Option<Uuid> }
pub struct PortalSlot { pub element_address: u16, pub voltag: Option<String>, /* ... */ }
```

The discovery algorithm joins library-reported drives (via READ ELEMENT STATUS with DVCID + CurData; §12.2.2) to host-side device nodes (via INQUIRY VPD page 0x80) by serial number. The result is a runtime mapping that does not need to be persisted — it is rebuilt fresh on each refresh.

### 6.2 Layer 2b — State-changing operations

**Status: live-tested on QuadStor + production MSL3040.**

Operations: `move_medium`, `load_drive`, `unload_drive`, `import_element`, `export_element`. Each goes through a **dirty-state machine** that handles completion-uncertain failures (transport errors, kernel I/O errors) without silently corrupting the in-memory snapshot.

Key types:

```rust
pub struct LibraryHandle<'a> { /* owns the locked transport for one library */ }
pub struct DriveHandle<'a> { /* owns a single drive's transport, borrows LibraryHandle dirty state */ }

pub enum DirtyCause {
    CompletionUnknown,   // CDB issued, response lost; physical state unknown
    PartialMove,         // operator-observable mid-operation interruption
    /* ... */
}
```

When a state-changing operation hits a `Transport` error, the snapshot is marked dirty with a precise cause; callers must `refresh()` before re-issuing.

### 6.3 Layer 2c — Hot-plug watcher

**Status: implementation complete; live-tested on akash 2026-05-18. Daemon-side consumption is Layer 5's job (unimplemented).**

Module: `remanence-library::watch` behind the `linux-udev` Cargo feature (requires `pkg-config` + `libudev-dev` system packages at build time). Cross-platform scaffolding (event types, trait, mock source, coalescer state machine) builds everywhere.

Subscribes to OS hot-plug events on SCSI subsystems and emits **coalesced notification bursts** (so a multi-device add/remove storm shows up as one burst, not 47 events). The watcher is notify-only; it never calls SCSI and never holds a `LibraryHandle`. The consumer (Layer 5 eventually) decides what to do — typically re-derive (`LibraryHandle::refresh()`) rather than incrementally update.

Until Layer 5 lands, callers can exercise the watcher directly via `rem watch` (feature-gated) or by subscribing programmatically through `LinuxUdevSource`, and manually call `LibraryHandle::refresh()` on bursts.

---

## 7. Layer 3a — Tape mechanism

**Status: implemented.** Implementation steps 9.0–9.8 merged to `main`; 303 unit/integration tests pass; step 9.9 (production MSL3040 live smoke + fixture capture) pending hardware window.

Lives in `crates/remanence-library/src/handle/tape_io/` (same crate as Layer 2, not a separate crate — earlier drafts proposed a split, codex review showed it was unworkable). The mechanism-only vocabulary of tape-side ops. No format awareness whatsoever — this layer pushes and pulls blocks at LBAs.

### 7.1 Public API

`DriveHandle<'a>` (already exposing `load()`/`unload()` from Layer 2b) gains:

```rust
impl<'a> DriveHandle<'a> {
    // Positioning
    pub fn rewind(&mut self) -> Result<(), TapeIoError>;
    pub fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError>;
    pub fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError>;
    pub fn position(&mut self) -> Result<TapePosition, TapeIoError>;

    // Data
    pub fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError>;
    pub fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError>;
    pub fn write_block_unpositioned(&mut self, buf: &[u8])
        -> Result<WriteUnpositionedOutcome, TapeIoError>;

    // SYNCHRONOUS barrier (IMMED never set); returns only after marks committed to media.
    pub fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError>;

    // Configuration
    pub fn read_config(&mut self) -> Result<TapeConfig, TapeIoError>;
    pub fn write_config(&mut self, cfg: TapeConfig) -> Result<(), TapeIoError>;
}

pub struct TapeConfig {
    pub block_size: BlockSize,         // Variable (default) or Fixed { size_bytes: u32 }
    pub compression: bool,             // false is the rem-chunked-v1 / rem-tar-v1 default
    pub max_block_size_bytes: u32,     // populated by read_config from READ BLOCK LIMITS
}

pub struct WriteOutcome  { pub bytes_written: u32, pub position_after: TapePosition, pub early_warning: bool, pub end_of_medium: bool }
pub struct WriteUnpositionedOutcome { pub bytes_written: u32, pub early_warning: bool, pub end_of_medium: bool }
pub struct WriteFilemarksOutcome { pub position_after: TapePosition, pub early_warning: bool, pub end_of_medium: bool }
pub struct SpaceResult   { pub records_moved: i64, pub position_after: TapePosition, pub hit_filemark: bool, pub hit_eod: bool, pub hit_bot: bool }
pub enum   SpaceKind     { Blocks, Filemarks, EndOfData }
pub struct TapePosition  { pub lba: u64, pub file_number: u32, pub block_number: u32, pub at_bop: bool, pub at_eop: bool, pub bpew: bool }
```

### 7.2 Block-size handling

LTO defaults to **variable-block** mode. Each WRITE writes one block of whatever size the buffer is; each READ returns one block of whatever size is on tape (truncated if the host buffer is too small — sense data signals it). Layer 3a defaults to variable-block. Rationale: matches POSIX tar; the 30-year-portability story depends on `dd if=/dev/nst0` working without configuration; avoids per-tape mode-mismatch hazards.

A format that wants fixed-block enforcement (e.g. streaming uncompressed video at a known frame size) can call `write_config(TapeConfig { block_size: Fixed { size_bytes: N }, .. })` at session open. Layer 3a does not second-guess.

**Per-generation block size caps** (drive enforces; after successful `read_config()`, Layer 3a maps WRITE `INVALID FIELD IN CDB` to `TapeIoError::BlockTooLarge` using the cached READ BLOCK LIMITS maximum):
- LTO-9: 16 MiB
- LTO-7/8: 8 MiB
- LTO-6: 1 MiB

### 7.3 Streaming, progress, cancellation

Layer 3a does not stream. `write_block` and `read_block` are synchronous, one-CDB-per-call. Streaming wrappers (e.g. `AsyncWrite` adapters) belong in Layer 5; mixing async at this layer would force a tokio dependency on every consumer.

`WriteOutcome.position_after` is the LBA of the next-block-to-write — progress publishing in Layer 5 needs no extra round-trip.

Layer 3c's raw fixed-block sink uses `write_block_unpositioned` for the hot sequential path: it seeds its cursor with one `position()` call, increments that cursor after each clean one-block write, and re-anchors at synchronous filemark barriers through positioned `write_filemarks`.

**Cancellation** (per §11.5): between CDBs is a safe point; inside a CDB is not. Once `write_block` issues the CDB and waits on `SG_IO`, there is no interrupt — the CDB completes or transport-errors; Layer 3a reports either, and the caller maps to `CompletedAfterCancel` or `CompletionUnknown`. Layer 3a does not expose its own cancel API.

### 7.4 Error model

```rust
#[derive(Debug, thiserror::Error)]
pub enum TapeIoError {
    CheckCondition(ScsiCheckCondition),     // CDB returned CHECK CONDITION; physical state KNOWN, not a dirty signal
    UnexpectedStatus(ScsiError),            // target status without CHECK CONDITION; command not accepted, not dirty
    InvalidRequest(ScsiError),              // caller request rejected before any CDB went out
    Transport(TransportError),              // SG_IO transport failure; completion UNKNOWN, dirty signal raised
    MalformedModeResponse(String),          // MODE SENSE/SELECT payload couldn't be parsed
    MalformedResponse(ScsiError),           // non-MODE response payload couldn't be parsed
    OperationFailed(String),                // adapter-level failure above SCSI; not a transport failure
    BlockTooLarge { requested: u32, limit: u32 },
    ReadBufferTooSmall { actual: u32, provided: u32 },  // block CONSUMED; caller must space(-1, Blocks) before retry
    FilemarkEncountered,                    // READ consumed a filemark boundary instead of returning data
    NoMedium,
    WriteProtected,
    DataProtect(ScsiCheckCondition),
}
```

**Dirty-state interaction**: `TapeIoError::Transport` from any data-path method calls a `pub(crate)` helper on the parent `LibraryHandle` that flips the snapshot dirty with cause `CompletionUnknown`. Drive position is tracked separately: after a completion-unknown drive transport failure, destructive writes are refused until `position()`, `locate()`, `rewind()`, or `space()` succeeds. `refresh()` / `rescan()` reconcile changer inventory, not tape-head position.

### 7.5 Implementation status (detail)

Steps 9.0–9.8 complete. 303 unit/integration tests + 2 `#[ignore]`-gated hardware tests (`quadstor_basic_smoke`, `quadstor_write_read_round_trip`). Step 9.9 (live MSL3040 smoke on a scratch LTO-9 tape) pending an operator window. Layer 3b can begin implementation now — Layer 3a's surface is stable enough.

---

## 8. Layer 3b — Body format

**Status: PARTIALLY IMPLEMENTED.** Crate `remanence-format` now contains the `rem-tar-v1` planner, writer, streaming reader, and generic format-driver scaffolding. Crate `remanence-stream` contains the first Layer 5-adjacent streaming orchestration surface. Remaining work is registry wiring, Layer 5 integration, and legacy foreign-format drivers.

### 8.1 Position in the stack

`rem-tar-v1` is the default native body format. It is a format driver advertising catalog scan, sequential restore, indexed file restore, range read, write, verify, and metadata-preserving capabilities (§8.3). It writes blocks via a `BlockSink` (in production wrapped by Layer 3c's `ParitySink`) and reads via a `BlockSource` (in production wrapped by Layer 3c's `ObjectParitySource`). It does not interact with parity or the SCSI layer directly.

### 8.2 The two-part on-tape layout

Remanence draws a hard line between two kinds of on-tape data:

1. **The catalog** — rem's own bookkeeping written at file-mark 0 of every tape (plus a copy at end-of-data). The catalog format is fixed by Remanence, versioned, **not pluggable**. (Bootstrap-problem prevention: a tape's catalog can't itself be written in an unknown format without an out-of-band channel. See §9.3 for the bootstrap format Layer 3c uses, which is the modern realization of this idea.)
2. **The body** — the actual archived data. The body format is **pluggable** via registered format drivers. Different tapes may use different body formats; the bootstrap records which.

Every tape, regardless of body format:

```
[BOT]
  Bootstrap (Layer 3c §9.3): scheme, tape UUID, filemark-map digest, optional SidecarEpochDirectory or ParityMapReference
  | filemark |
  Object 0 (pax tar, rem-tar-v1)
  | filemark |
  Object 1 (pax tar)
  | filemark |
  Parity sidecar (epoch N)
  | filemark |
  Object 2 (pax tar)
  ...
  Bootstrap (mid-tape, content-driven placement per §9.5)
  ...
  Final bootstrap (is_final_map=true)
  [EOD]
```

The bootstrap records each object's `tape_file_number`, body-format identifier, and per-object metadata (caller-object-id, content hash, etc.).

### 8.3 Format drivers, sources, and capabilities

The registry stores executable format drivers, not declarative field specs. A format spec can describe headers and fields, but the driver owns the streaming state machine: scan, validation, indexing, payload streaming, damage reporting, and write behavior where supported.

Every driver exposes:

- `FormatDescriptor`: id, version, source requirement, and capabilities.
- `ArchiveReader`: normalized scan and restore events for readable formats.
- Optional native writer support for formats that can create Remanence objects.

#### Source requirements

Drivers must declare the source they need before parsing:

| Source requirement | Meaning |
|---|---|
| `ObjectBlocks` | Object-local fixed blocks supplied by `BlockSource`, normally through Layer 3c's `ObjectParitySource`. Native Remanence formats use this. |
| `PhysicalTapeRecords` | Physical tape records and filemarks before Remanence object decoding. Legacy/foreign tape readers use this when record framing is part of the format. |
| `ByteStreamDump` | A dump file or equivalent byte stream. Useful for offline import and tests. |
| `ObjectBytes` | Already-materialized object bytes. Compatibility path; not the production streaming restore path. |

The physical tape source boundary belongs in the shared library/daemon surface, not inside Layer 3c. Layer 3c remains body-format-agnostic and continues to expose object-local `BlockSource` for native objects.

#### Capabilities

Capabilities are explicit flags, not tier names:

| Capability | Meaning |
|---|---|
| `catalog_scan` | Enumerate entries without restoring all payload bytes. |
| `sequential_restore` | Stream entries in archive order. |
| `indexed_file_restore` | Seek to an individual file after a manifest/index exists. |
| `range_read` | Stream a byte range within one file. |
| `write` | Create new objects in this format. |
| `verify` | Validate format-level checksums. |
| `damage_events` | Surface damaged byte ranges without aborting the whole stream. |
| `metadata_preserving` | Preserve archival metadata beyond path and bytes. |

The daemon checks capabilities before tape motion. Unsupported operations fail early.

To claim indexed file restore, the driver must provide stable `FileId` values and adapter-owned index state sufficient to locate an entry. To claim range reads, it must provide per-range addressing and be decodable from the advertised boundaries. Forward scan should remain possible even when an optional index is missing or corrupt.

### 8.4 Native and foreign formats

Native body formats operate inside Remanence object tape files. They read through `BlockSource`, write through `BlockSink`, and let Layer 3c own physical filemarks, sidecars, and parity.

Foreign tape formats decode an existing non-Remanence tape or dump. They may need physical tape records, filemarks, drive block-size configuration, or format-specific logical block splitting before they can emit normalized archive events. They are normally read-only and feed migration/import pipelines rather than writing their own new Remanence tape bodies.

### 8.5 Format registry

Two registration mechanisms:

1. **In-tree formats** — registered explicitly in `main` by the daemon binary.
2. **External formats** — loaded as Rust dependencies and registered the same way. Out-of-tree formats require a recompile-and-redeploy. **No dynamic plugin loader** — loading binary plugins into a long-lived daemon with hardware access widens the attack surface beyond convenience-justified bounds.

On tape mount, the daemon reads the bootstrap, finds each object's body-format id, and looks up the corresponding driver. Unknown format ids return `UnknownFormat`; the tape remains intact. Older formats stay registered as long as any tape in the fleet uses them — formats are a forever decision.

### 8.6 Built-in formats

| Format id | Source | Capabilities | Notes |
|---|---|---|---|
| `pax-tar-v1` | `ObjectBlocks` | catalog scan, sequential restore, indexed file restore via sidecar index, write | POSIX pax tar — maximum-portability option. Forty-five years of stable tooling. Range reads are not native. |
| `rem-tar-v1` | `ObjectBlocks` | catalog scan, sequential restore, indexed file restore, range read, write, verify, metadata-preserving | The default. Constrained pax tar subset with chunk alignment for PFR. See §8.7. |
| `remanence-tar-legacy` (planned, see below) | `PhysicalTapeRecords` / `ByteStreamDump` | catalog scan, sequential restore, verify where source permits | Reader for standard pax-tar tapes written by dwara v2 / other tools. **Read-only.** Wraps the Rust `tar` crate where possible. |
| `remanence-bru` (planned, see below) | `PhysicalTapeRecords` / `ByteStreamDump` | catalog scan, sequential restore, verify, damage events; indexed restore after scan/index | Reverse-engineered reader for BRU/BRU-PE tapes (Tolis Group, defunct — RE is legal). **Read-only.** |

The two planned legacy readers exist to support migration from existing production tape archives written by dwara v2 (TAR) and older BRU systems. Both are reader-only — no write path. Sutradhara's migration strategy depends on these. BRU's 2048-byte logical block size must not be confused with the physical tape record size; the BRU driver reads physical records safely and splits them into logical BRU blocks.

`rem-chunked-v1` (originally specified as the default in spec-v0.3) has been **superseded** by `rem-tar-v1`. The capability surface and tier semantics are unchanged; only the on-tape wire format differs. Rationale: the 30-year-portability argument is stronger when the on-tape body is byte-compatible with standard `tar`.

### 8.7 rem-tar-v1 specification

`rem-tar-v1` is a constrained subset of POSIX pax tar with Remanence-specific extensions in pax extended headers. The constraints exist to satisfy 3c's parity layer (chunk-aligned to tape block boundaries) and to enable efficient byte-range access. The unconstrained tar stream remains extractable by any pax-aware tar tool, with the chunk metadata simply ignored.

#### 8.7.1 Three address spaces

Layer 3c defines three address spaces; `rem-tar-v1` lives in one of them:

- **`PhysicalLba` / `TapePosition`**: physical tape address `(tape_file_number, block_within_file)`. 3a / 3c concern.
- **`ParityDataOrdinal`** (3c-internal): logical sequence numbering only the protected data records, skipping filemarks. RS epochs defined over this. rem-tar-v1 never sees it.
- **`BodyLba`** (per-object): the logical block stream within a single object archive. Starts at 0 at each new object; paired with the object's `tape_file_number` to form a complete address. **rem-tar-v1 stores and computes only per-object `BodyLba`.** Because each object archive is a clean pax tar tape file with no parity blocks inside it (parity lives in separate sidecar tape files), BodyLba is contiguous with no gaps.

#### 8.7.2 Object framing

One Remanence object is one complete pax tar archive occupying one tape file, terminated by a tape filemark.

```
| object 0 (pax tar) | object 1 (pax tar) | parity sidecar | object 2 (pax tar) | ... | EOD
```

Each archive begins with a **global pax header** (typeflag `g`) carrying object-level keywords, and ends with the standard tar EOF (two zero records) followed by a fixed-block zero-fill of the remaining partial block, followed by a tape filemark written by 3c.

#### 8.7.3 Block alignment

`rem-tar-v1` aligns the **start** of each file's data to a per-object `BodyLba` block boundary (default `chunk_size = 256 KiB`). This is the one structural constraint beyond plain tar, and it is implemented in a way that keeps the byte stream a fully valid pax tar archive.

The critical correctness rule: **tar validity is non-negotiable.** No zero padding within a file's payload. No inflated tar header sizes. Both break standard tar extraction.

How alignment works:

- **File-data start alignment.** The pax extended header immediately preceding the file is sized so that the header + the file's ustar header together end exactly on a block boundary. Pax extended headers have arbitrary length (length-prefixed `keyword=value` records), so the writer adds a single `REMANENCE.pad=<spaces>` record to the *real* pax header of the next file, padding it to the needed size. This is a legitimate pax header for the following entry, never a standalone padding member.
- **File-data end.** Exact byte length in the ustar header, followed only by tar's normal 512-byte-record padding. The last block of a file may contain the file's tail bytes plus the start of the next tar structure. The reader trims to the exact file size from the catalog/manifest; never relies on block-level zero padding.
- **Final block zero-fill (fixed-block media).** After the tar EOF records, the writer zero-fills the remainder of the final `chunk_size` block. This is **tar-safe**: it occurs after archive EOF, where standard tar already stops; the trailing zeros are never interpreted as data. This is the *only* block-level zero-fill in the format.

The pax-padding equation (§7.6 in rem-tar-v1-design.md, normative):

```
O + 512 + roundup512(P) + 512 ≡ 0 (mod S)
```

where `O` is the byte offset of the pax header start, `P` is the whole pax body (rounded as a unit), `S` is `chunk_size`, and the two `512`s account for the pax extended header's own ustar record AND the file's ustar header. Iterate-to-fixed-point to handle the self-referential pax `<len>` field across decimal-digit boundaries.

#### 8.7.4 REMANENCE.* pax keywords

**Global header (typeflag `g`):**
- `REMANENCE.format_id=rem-tar-v1`
- `REMANENCE.schema_version=1.0`
- `REMANENCE.object_id=<uuid>`
- `REMANENCE.caller_object_id=<string>` (orchestrator-supplied opaque ID)
- `REMANENCE.chunk_size=262144`
- `REMANENCE.metadata_preservation=<archival|full|minimal>`
- `REMANENCE.encryption=<flag>`
- `REMANENCE.write_timestamp=<RFC3339>`

**Per-file extended header (typeflag `x`):**
- `path=<long path if needed>` (standard pax)
- `size=<exact size in bytes>` (standard pax)
- `mtime=<...>` (archival / full only)
- `REMANENCE.file_id=<uuid>`
- `REMANENCE.file_sha256=<hex>` ← **the per-file content hash that makes the rebuildable index work**
- `REMANENCE.chunk_count=<n>`
- `REMANENCE.executable=<bool>` (archival / full)
- `REMANENCE.compression=none` (v1 only supports `none`)
- `REMANENCE.pad=<spaces>` (sizes this header for block alignment of the following file's data)

**Manifest's own pax keys:**
- `path=_remanence/manifest.cbor`
- `size=<exact manifest size>`
- `REMANENCE.file_id=<manifest uuid>`
- `REMANENCE.is_manifest=true`
- `REMANENCE.file_sha256=<hex of manifest content>`
- `REMANENCE.chunk_count=<m>`
- `REMANENCE.pad=<spaces>`

#### 8.7.5 Object manifest (`_remanence/manifest.cbor`)

Each object's last regular file (before the tar EOF) is a CBOR-encoded manifest. Canonical CBOR: sorted keys, definite lengths, no encoder-dependent ordering — required so `manifest_sha256` is implementation-independent.

Structure (CBOR, conceptual schema):

```
ObjectManifest {
    schema_version: u16,
    object_id: UUID,
    caller_object_id: String,
    chunk_size: u32,
    file_entries: [FileEntry],
    external_references: [ExternalReference],    // external symlinks; §8.7.7
    object_metadata: { ... },
}

FileEntry {
    file_id: UUID,
    path: String,
    size_bytes: u64,
    file_sha256: bytes,
    first_chunk_lba: Option<u64>,        // per-object BodyLba; absent iff chunk_count == 0
    chunk_count: u32,
    executable: Option<bool>,
    metadata_preservation_data: { ... },
}

SymlinkEntry, HardlinkEntry, DirectoryEntry, ExternalReference: see §8.7.7.
```

**The manifest excludes itself** (`FileEntry` array lists payload files only, never `_remanence/manifest.cbor`). The manifest's own identity/hash live in its pax header and the bootstrap row.

**Direct-read addressing:** the bootstrap stores `manifest_first_chunk_lba` + `manifest_size_bytes` + `manifest_chunk_count` per object so a reader can LOCATE directly to the manifest without scanning the whole archive. This is what enables Sutradhara's `enumerate()` to walk every object on a tape efficiently.

**Chain of trust:** bootstrap → `file_sha256` per file → `manifest_sha256` (whole-manifest hash, stored on the bootstrap row, plus the manifest's own pax `REMANENCE.file_sha256` keyword as a within-tape cross-check).

#### 8.7.6 String encoding policy

All strings in `rem-tar-v1` are **UTF-8 encoded**. Strict, format-wide, covering:
- POSIX pax `path` and standard string keywords
- All `REMANENCE.*` keyword names and values
- All manifest text strings (paths, UUIDs as strings, timestamps, xattr keys/values)
- All CBOR text strings (CBOR major type 3 requires UTF-8 per RFC 8949)

**Pre-write validation** (§9.0 of the source doc) refuses to write any object containing a non-UTF-8 filename. The writer never silently transliterates. The catalog DB charset is required to be UTF-8 (Postgres `ENCODING 'UTF8'`, SQLite default, MySQL/MariaDB `utf8mb4`).

Failure mode this prevents: BRU/TOLIS production deployments have stored filenames in mixed encodings; ingest into a narrower-charset DB silently transliterated, and restore lookups failed because the in-catalog name didn't match the on-tape name. Strict UTF-8 at write time + UTF-8-required catalog eliminates the encoding boundary entirely.

#### 8.7.7 Symlinks, special files, directories

**Symlinks** classified by textual analysis of target path (not by `stat()`-ing the target — staging systems often don't have the target's filesystem):

| Classification | Definition | Default action |
|---|---|---|
| Internal | Target resolves textually to a path within the archive root, AND that path is in the file list. | Archive normally. |
| External-absolute | Target is absolute, outside the archive root. | Archive verbatim; record in `ExternalReferences`. |
| External-relative | Relative target that, when resolved, escapes the archive root. | Same as External-absolute. |
| Internally-broken | Target resolves to a path within the archive root, but the path is NOT in the file list. | **Reject** by default. |

`SymlinkPolicy` enum: `Default` (reject internally-broken, accept external), `Strict` (reject any dangling symlink — self-contained archives), `Permissive` (accept everything — emergency archives).

**Cycle immunity:** the default `SymlinkPolicy` never follows symlinks during traversal; the writer cannot exponentially blow up an archive even when the source tree contains symlink cycles (e.g. FCP's `.fcpcache/` self-referential links that historically caused BRU to write hundreds of GB of content 20+ times before hitting a depth limit). The recursion-bomb failure mode is structurally impossible.

**Restore never fails on missing symlink targets.** The archive's job is byte-faithful reproduction of the symlink itself, not its target.

**Special file types:**
- Hard links → `HardlinkEntry`
- Directories → `DirectoryEntry` (metadata preservation)
- Device nodes / FIFOs / sockets → **rejected by default**; allowed only in `system-backup` mode

**Empty files** (`size=0`): `chunk_count=0`, no chunk data, no alignment needed.

#### 8.7.8 Metadata preservation tiers

`MetadataPreservation` enum on the write session:

- **`Minimal`**: path + content only.
- **`Archival`** (default): path, content, mtime, executable bit, xattrs. Deliberately omits uid/gid/uname/gname and non-executable mode bits (these don't transfer faithfully across systems or decades and create false fidelity expectations at restore time).
- **`Full`**: everything `Archival` preserves plus uid/gid/uname/gname and full mode bits.

USTAR header sanitization in non-`Full` modes ensures a root-run standard tar restore cannot apply ownership the format claims not to preserve.

#### 8.7.9 Validation, sanity, spooling

**Pre-write validation** (§9.0): UTF-8, symlink classification, entry-count cap (default 10 million entries per object), inode-repetition heuristic (default: an inode appearing more than 100 times in the input list when not declared as hard links triggers a walker-cycle warning).

**Large-file hashing/spooling** (§9.2): immutable or snapshot-backed sources use a two-pass reread with no temp copy; mutable sources spool exact bytes during pass 1 and write the spool during pass 2. Pass 2 re-hashes and compares to pass 1 before the object can commit.

**Compression**: format-level compression removed in rem-tar-v1 v0.7. Reframed as an orchestrator-level function. The workload is video (already-compressed codecs), where format-level zstd buys little and sometimes expands; and compression was responsible for a disproportionate share of wire-level complexity. The `compression` field is retained as a reserved enum that only takes `none` in v1, so a future v1.x could reintroduce format-level compression without a format-version bump if evidence justifies it.

#### 8.7.10 Reader contract

- **LOCATE-by-file**: bootstrap has `(tape_file_number, first_chunk_lba)` per file (or via manifest direct-read). Reader positions to the object's tape file, then to BodyLba.
- **Byte-range reads**: `first_chunk_lba + (start_byte / chunk_size)` directly addresses the chunk containing `start_byte`. Read minimum chunks covering the range; trim head/tail. The short-last-chunk case requires accounting for `file_size mod chunk_size`; see `pfr-reference.md` §4.4 for the worked example.
- **Forward-scan fallback** (catalog-corrupt): use tar header `size` + 512-byte padding to advance. Each object is a standalone pax tar archive, so this works without any rem-specific knowledge.

#### 8.7.11 Writer / BodyBlockWriter contract

- Streams the tar byte stream into `chunk_size` blocks via `BlockSink::write_block`.
- Flushes the final partial block (zero-filled) only after the object's tar EOF, via `BodyBlockWriter::finish_after_tar_eof()`.
- 3c's `ParitySink::finish_object()` then writes the filemark and any completed-epoch sidecars, and asserts the body buffer was empty (no layer leak, no double flush).
- Capacity reservation: rem-tar-v1 must compute `projected_size_blocks` upper bound before `begin_object` (3c §9.7 capacity reserve). The safest implementation is a counting-mode dry run that uses the same header-sizing/padding code as the writer.
- Write-session setup: drive `write_config` (fixed block size or variable, compression-off) happens **once** before `ParitySink::new` and the BOT bootstrap — not inside the per-object sequence.

#### 8.7.12 Interaction with Layer 3c session lifecycle

(*Updated to reconcile with 3c v0.7.2; the source `rem-tar-v1-design.md` v0.9.3 was pinned to 3c v0.4.4 and did not cover the post-v0.4.4 additions below.*)

Layer 3c v0.7 added explicit session-lifecycle calls on `ParitySink`:

- `ParitySink::checkpoint()` — asserts no object is open; writes a non-final prefix bootstrap; commits a `Control` bundle; leaves the tape resumable. **Mid-object checkpoint is rejected.** rem-tar-v1 cooperates by ensuring callers do not invoke `checkpoint()` between `begin_object` and `finish_object`.
- `ParitySink::finish()` — terminal close: writes the final bootstrap (`is_final_map=true`), closes the final partial epoch, returns `TapeGeometry`. Tape is closed forever; subsequent appends require a fresh tape.
- Commit point: `ParitySink::finish_object()` writes synchronous filemark then a journal fsync (`TapeFileJournal::commit_bundle`). **The fsync is the durable commit barrier.** rem-tar-v1's notion of "object committed" is exactly the return of `finish_object()`.

Layer 5 has an obligation to call `checkpoint()` before any SCSI `UNLOAD` of a tape it intends to resume. Otherwise the tape enters unclean-end state — no data loss, but a slower tape-only forward-validation scan on remount.

#### 8.7.13 30-year fallback

The on-tape body is a clean pax tar archive. Standard `tar` (with `-b 512` for the 256 KiB block size: `512 × 512 B = 256 KiB`) extracts files byte-correct. Pax-aware tools see and skip `REMANENCE.*` keywords (POSIX requires unknown vendor keywords to be ignored).

---

## 9. Layer 3c — Tape parity

**Status: substantially implemented in `crates/remanence-parity` (~42k LOC, 19 source files).** Active hardening driven by a codex implementer cron (`scripts/codex-layer3c-implementer.sh`). Spec: implementation-ready v0.7.2 (May 26 2026). Five open items at §9.10.

### 9.1 Architectural model

Reed-Solomon parity over GF(2⁸), Cauchy generator, designed for catalog-less recovery of contiguous tape damage up to ~512 MiB at default scheme. Per-object filemarks coexist with epoch-spanning parity; parity is written as **separate sidecar tape files** at object boundaries, never inside object archives. This gives:

- Clean per-object pax tar archives (standard `mt`/`tar` navigation works).
- Parity that accumulates freely across objects of any size without staging.
- Recovery scoped to epoch, not whole tape.

### 9.2 Three address spaces

(Repeated from §8.7.1 for the parity perspective.)

- **`TapePosition`** (physical): `(tape_file_number, block_within_file)`, plus filemark map. Tape is a sequence of filemark-delimited tape files: object archives, parity-epoch sidecar files, bootstrap tape files.
- **`ParityDataOrdinal`** (3c-internal): logical sequence numbering only the protected data records, in order, across object archives, **skipping filemarks**. Object A's blocks get ordinals 0..a; the filemark after A gets no ordinal; object B's blocks continue at a+1. RS neighborhoods ("parity epochs") are defined over this space — so a stripe can span the filemark between two objects.
- **`BodyLba`** (per-object): the per-object stream rem-tar-v1 sees.

### 9.3 Reed-Solomon scheme

**`rs-cauchy-gf256-v1`** (the only scheme in v1):

- GF(2⁸), polynomial `0x11D`.
- Cauchy generator: `G[j][i] = 1 / (X_j XOR Y_i)`.
- Encoder owned in-tree (`reed-solomon-erasure` is an optional accelerator gated by an Appendix-A byte-identical conformance test).
- Incremental parity accumulation (Option B): keep only `S × m` parity accumulators (~512 MiB at default), update them per arriving data shard via `accumulate(stripe, index, &shard)`.

**`ParityScheme` type:**

```rust
pub struct ParityScheme {
    pub id: SchemeId,                    // "rs-cauchy-gf256-v1"
    pub data_blocks_per_stripe: u16,     // k; default 128
    pub parity_blocks_per_stripe: u16,   // m; default 4
    pub stripes_per_epoch: u32,          // S; block-size-aware
}
```

Default (block-size-aware): `k=128, m=4, S=stripes_for_tolerance(block_size, 512 MiB, m)`. At 256 KiB blocks → `S=512`; at 1 MiB blocks → `S=128`. Conservative alternative: `k=64, m=6, S=stripes_for_tolerance(block_size, 384 MiB, m)`.

Validation: `k, m ≥ 1`; `k + m ≤ 255` (GF(2⁸) limit); `S ≥ 1`; epoch size = `S × (k + m) × block_size` is bounded (no epoch larger than memory).

Implementation status (2026-06-10): `ParitySink` still uses a volatile
RAM-backed pending-sidecar spool for completed epochs inside the active object.
The object-start capacity reserve bounds how many completed sidecars may be
queued before `finish_object`; the crash-durable local-disk parity spool remains
future work.

### 9.4 Public interface (the surface 3b and 5 see)

#### `BlockSink` / `ParitySink` — what 3b writes through

```rust
pub struct ParitySink<'a> { /* wraps &'a mut dyn RawTapeSink */ }

impl<'a> ParitySink<'a> {
    pub fn new(
        inner: &'a mut dyn RawTapeSink,
        journal: &'a mut dyn TapeFileJournal,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        spool_config: SpoolConfig,
    ) -> Result<Self, ParityError>;

    pub fn begin_object(&mut self, projected_size_blocks: u64)
        -> Result<u32 /* tape_file_number */, ParityError>;
    pub fn finish_object(&mut self) -> Result<ObjectCloseResult, ParityError>;
    pub fn write_bootstrap(&mut self) -> Result<u32, ParityError>;
    pub fn checkpoint(&mut self) -> Result<CheckpointResult, ParityError>;
    pub fn finish(self) -> Result<TapeGeometry, ParityError>;

    // BlockSink impl:
    pub fn write_block(&mut self, buf: &[u8]) -> Result<BodyWriteOutcome, ParityError>;
}
```

`BodyWriteOutcome` carries `BodyPosition { tape_file_number, body_lba }` plus EOM/early-warning flags from the underlying tape.

#### `BlockSource` / `ParitySource` / `ObjectParitySource` — what 3b reads through

```rust
pub struct ParitySource<'a> { /* wraps &'a mut dyn RawTapeSource */ }

impl<'a> ParitySource<'a> {
    pub fn new(
        inner: &'a mut dyn RawTapeSource,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        filemark_map: ScopedFilemarkMap,
    ) -> Result<Self, ParityError>;

    pub fn open_object(&mut self, tape_file_number: u32, trust: OpenTrust)
        -> Result<ObjectParitySource<'_, 'a>, ParityError>;
    pub fn recover_region(&mut self, req: RecoveryRegionRequest)
        -> Result<impl Iterator<Item = RecoveredBlock>, ParityError>;
    pub fn recover_ordinal_range(&mut self, ordinals: Range<u64>)
        -> Result<impl Iterator<Item = RecoveredOrdinalBlock>, ParityError>;
}

pub struct ObjectParitySource<'p, 'a> { /* ... */ }

impl<'p, 'a> ObjectParitySource<'p, 'a> {
    pub fn recover_block_at(&mut self, body_lba: u64) -> Result<Vec<u8>, ParityError>;
}

impl<'p, 'a> BlockSource for ObjectParitySource<'p, 'a> {
    fn locate(&mut self, body_lba: u64) -> Result<BodyPosition, FormatError>;
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, FormatError>;
    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, FormatError>;
    fn position(&mut self) -> Result<BodyPosition, FormatError>;
}
```

#### `RawTapeSource` / `RawTapeSink` — the Layer 3a boundary

```rust
pub enum RawReadOutcome {
    Block      { bytes: usize, position_after: PhysicalPositionHint },
    Filemark   { position_after: PhysicalPositionHint },
    EndOfData  { position_after: PhysicalPositionHint },
}

pub enum RawWriteOutcome {
    WroteBlock    { position_after: PhysicalPositionHint, early_warning: bool, end_of_medium: bool },
    WroteFilemark { position_after: PhysicalPositionHint, early_warning: bool, end_of_medium: bool },
}

pub trait RawTapeSource {
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), FormatError>;
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), FormatError>;
    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, FormatError>;
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, FormatError>;
    fn position(&mut self) -> Result<PhysicalPositionHint, FormatError>;
}

pub trait RawTapeSink {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, FormatError>;
    fn write_filemark(&mut self) -> Result<RawWriteOutcome, FormatError>;  // SYNCHRONOUS commit barrier
    fn position(&mut self) -> Result<PhysicalPositionHint, FormatError>;
}
```

The production `DriveHandleRawSink` writes fixed blocks through Layer 3a's unpositioned WRITE path and maintains the physical `position_after` hint locally for clean sequential writes. It still uses positioned `write_filemarks` for commit barriers, so catalog-visible tape-file boundaries come from the drive's own `READ POSITION` result.

### 9.5 Bootstrap placement policy

```rust
pub struct BootstrapPlacementPolicy {
    pub bundles_per_bootstrap: u32,             // staleness floor: committed bundles
    pub ordinals_per_bootstrap: u64,            // staleness floor: newly-protected ordinals
    pub eom_taper: Vec<(f64, u32)>,             // [(fraction_remaining, divisor)]
    pub min_physical_separation_blocks: u64,    // no two bootstraps closer than this
}
```

Content-driven, not fixed positions. Mandatory at BOT. Between BOT and `finish()`, emit a non-final bootstrap when `bundles_since_last >= bundles_per_bootstrap` OR `protected_ordinals_since_last >= ordinals_per_bootstrap`, at the next object boundary, subject to `min_physical_separation_blocks`. Apply EOM taper divisors as remaining capacity shrinks (e.g., halve the floor in last 10%, quarter it in last 1%). `checkpoint()` always writes a bootstrap and resets staleness counters. Mandatory final bootstrap at `finish()` with `is_final_map=true`.

**Bootstrap discovery:** BOT → hints (journal, MAM, catalog, operator positions) → full filemark scan (catalog-less fallback). Select first-valid for scheme; select authoritative (final, else highest sequence, else widest prefix) for map validation.

**Block-size fallback (v0.7.2):** finite, ordered candidate list `[256 KiB, 512 KiB, 1 MiB]` by default. Implementations try configured size first, then candidates; no open-ended brute force. Reconfigure drive for each candidate before read.

### 9.6 On-tape parity structures

#### Sidecar tape file format

Fixed-block sidecar with header/index blocks + raw parity-shard blocks, with v0.5 replication (primary header at front, tail header copy at end, one-block footer locator):

```
[block 0: primary header + inline index entries + per-block CRC]
  magic (8 bytes, HMAC-SHA256(tape_uuid, b"REM\x00PAR\x01")[0..8])
  epoch_id, k, m, S, block_size, schema_version=2
  protected_ordinal_start, protected_ordinal_end_exclusive  (half-open)
  logical_shard_count (= S×k), real_data_shard_count (D)
  parity_block_count (P = S×m), data_crc_count (D)
  shard_index_block_count (H), inline_index_entry_bytes
  copy_kind, sidecar_total_block_count
  primary/tail/footer block indices, canonical_metadata_hash
  header_crc64 (covers header before this field)
  [inline 16-byte parity index entries + 8-byte data-CRC entries]
  block0_crc64 (LAST field, covers header + inline entries + zero-fill)

[blocks 1..H-1: spilled shard index, per-block CRC]
[blocks H..H+P-1: parity shards (exactly block_size bytes each)]
[blocks H+P..H+P+H-1: TAIL header + index copy (byte-identical to primary except copy_kind=1)]
[block H+P+H: SidecarFooter — one block]
  magic (HMAC-SHA256(tape_uuid, b"REM\x00PARFOOT\x01")[0..8])
  sidecar_footer_version=1, sidecar_header_block_count (H)
  epoch_id, protected_ordinal_start/end_exclusive, parity_shard_block_count (P)
  sidecar_total_block_count (= 2H + P + 1)
  primary/tail header block indices, canonical_metadata_hash
  footer_crc64, footer_block_crc64
```

All CRCs are **CRC-64/XZ** (polynomial `0x42F0E1EBA9EA3693`, refin=refout=true, init/final=`0xFFFFFFFFFFFFFFFF`).

#### Bootstrap tape file format

One block:

```
magic (8 bytes: 'R','E','M',0x00,'B','O','O',0x01)
schema_major, schema_minor (u16be)
flags (u32be; bit 0 = no-parity tape)
tape_uuid (16 bytes)
block_size_bytes (u32be)
sequence (u32be; sink-owned, monotonic)
cbor_payload_len (u32le)
crc64_header (u64le; covers 0x00..0x2C including cbor_payload_len)
[CBOR payload: ParitySchemeRecord + FilemarkMapDigest + optional SidecarEpochDirectory | ParityMapReference]
crc64_payload (u64le)
[zero-fill to block_size]
```

#### Filemark map and canonical digest

Per tape file: `(tape_file_number, kind, block_count, first_parity_data_ordinal | protected_ordinal_range, epoch_id)`. `kind ∈ {object, parity_sidecar, bootstrap, parity_map}`. Maps `(tape_file_number, body_lba) → ParityDataOrdinal → (stripe_address, parity_shards_in_sidecar)`.

**Canonical map projection (digest input):** SHA-256 of canonical CBOR over ascending `tape_file_number` order; includes structural fields only (number, kind, block_count, ordinal ranges); **excludes** content hashes, volatile fields, copy-kind. Non-circular: bootstrap's own entry appears by structure only, so the final bootstrap can digest-include its own entry.

#### `SidecarEpochDirectory` and `parity_map`

When the directory fits in a single bootstrap block: inline. When it overflows: spilled to an external `parity_map` tape file with primary/tail/footer replicated structure. Bootstrap carries either inline directory or `ParityMapReference`. Directory attestation: per-tape-file `(epoch_id, protected_ordinals, block_count, canonical_metadata_hash)`. At scan reconstruction, directory overlays scanned tape-file classes and restores classification if primary sidecar header is damaged. A bootstrap `ParityMapReference` supplies structural classification for the referenced control tape file (`tape_file_number`, `block_count`, `kind=parity_map`) even if the `parity_map` payload is unreadable; the directory content inside that `parity_map` is used only after payload/hash validation.

#### Committed bundles

Atomic journal unit. `CommittedBundleKind`:

- **`Object`** — object archive + all its completed-epoch sidecars (the common case).
- **`Control`** — `write_bootstrap()` or `checkpoint()` produced this.
- **`ResumeSidecars`** — sidecars emitted during a resume / rebuild path.
- **`Finish`** — final sidecar (if open epoch has D > 0 data) + final bootstrap.

Commit ordering: blocks → synchronous filemark → journal fsync. **Hard invariant after any committed bundle:**

```
total_committed_ordinals − highest_protected_ordinal < data_ordinals_per_epoch
```

(At most one partial epoch's worth of data is uncovered by parity at any moment.)

### 9.7 Persistence — `TapeFileJournal` + default `FileTapeFileJournal`

```rust
pub struct TapeFileEntry {
    pub tape_file_number: u32,
    pub kind: TapeFileKind,                              // Object | ParitySidecar | Bootstrap | ParityMap
    pub block_count: u64,
    pub physical_start_hint: Option<u64>,
    pub object_id: Option<ObjectId>,
    pub first_parity_data_ordinal: Option<u64>,
    pub epoch_id: Option<u64>,
    pub protected_ordinal_start: Option<u64>,
    pub protected_ordinal_end_exclusive: Option<u64>,
    pub canonical_metadata_hash: Option<[u8; 32]>,
}

pub struct CommittedBundle {
    pub kind: CommittedBundleKind,
    pub entries: Vec<TapeFileEntry>,
    pub highest_protected_ordinal: u64,     // watermark AFTER this bundle
    pub total_committed_ordinals: u64,      // T AFTER this bundle
}

pub trait TapeFileJournal {
    fn tape_uuid(&self) -> [u8; 16];
    fn commit_bundle(&mut self, bundle: &CommittedBundle) -> Result<(), JournalError>;  // fsync = commit
    fn load_committed(&self) -> Result<CommittedState, JournalError>;                    // drops torn trailing record
}
```

**`FileTapeFileJournal` default:** single append-only file per tape `<journal_dir>/<tape_uuid>.remjournal`. Layout:

```
[magic | version | tape_uuid | block_size | scheme_consistency_copy | header_crc64]
[len | CBOR(bundle) | record_crc64]*
```

No server. Shared-reader vs exclusive-writer flock; readers never truncate torn tails. **fsync is the commit point** — `commit_bundle` returns only when durable.

**Journal durability precondition** (§10.6 source): journal pinned to the same trusted local volume as the parity spool. Network FS, tmpfs, or untrusted-flush device rejected at session open (`JournalError::UntrustedVolume`). fsync honored (Linux: write-through or write-back with fua=1).

**Tape-only resume** (§7.8 source): if journal lost, a later session reconstructs committed bundles from the last on-tape bootstrap by forward-validating tape files (each object required to be followed by the sidecars it completed). Journal-less suffix validation seeds `current_epoch_fill` explicitly from `T - W` where `T` is `directory_scope_total_data_ordinals` and `W` is `directory_scope_highest_protected_ordinal`. Slower than journal resume, always possible.

**Authority hierarchy:**
1. **Tape itself** (bootstraps + sidecar directory / `parity_map`, self-describing via scan + digest validation).
2. **Journal** (fast resume / commit record, durable local file).
3. **Optional external store** (SQLite query mirror in Layer 4, or orchestrator RDBMS) — downstream mirror only, never a 3c commit boundary.

### 9.8 Session lifecycle

**`checkpoint()` vs `finish()`:**

- `checkpoint()`: assert no object is open; **do NOT** close the partial epoch (stays open and rebuilds on resume); write a non-final prefix bootstrap; commit a `Control` bundle; reset staleness counters; return resumable state.
- `finish()`: close final partial epoch (zero-pad, emit final sidecar if D > 0, update watermark to EOD); write final bootstrap (`is_final_map=true`); commit `Finish` bundle; return `TapeGeometry`; **terminal** (tape closed forever).

**Layer 5 obligation:** MUST call `checkpoint()` and observe return before SCSI `UNLOAD` of a resumable tape. Otherwise the tape enters unclean-end state — no data loss, but slower tape-only resume scan on remount.

**Clean vs unclean end:** clean = `checkpoint()` or `finish()` returned success. Unclean = crash, power loss, cartridge pulled mid-session. Unclean takes the slower resume path.

**Crash recovery:** append point = trailing filemark of last journal-committed tape file. On resume, rebuild `FilemarkMapBuilder` from `journal.load_committed()`; if `W < T` (object data without sidecar yet), perform open-epoch rebuild: re-read committed blocks `[W, T)`, re-accumulate parity, emit `ResumeSidecars` bundles, load partial epoch as live state. After rebuilt epoch, continue appending normally.

### 9.9 Hard preconditions

**Drive hardware compression**: MUST be disabled and **read-back verified** before any parity-protected write. Layer 3a configures and verifies via MODE SELECT / MODE SENSE before BOT bootstrap. If compression cannot be disabled/verified, write session fails before any bootstrap (`DriveCompressionEnabled` / `DriveCompressionModeUnknown`). Bootstrap and journal record `drive_compression=false`. A tape recording `drive_compression=true` is refused for 3c recovery.

**Object size / no-spanning preflight** (§7.5 source): before writing any object block, compute full footprint (object + filemark + sidecars + bootstraps + reserve) against empty-tape capacity. If object won't fit on empty tape, reject with `BeginObjectError::ObjectTooLargeForEmptyTape` before any write. Objects do not span tapes.

**Capacity reserve** (§7.5 source): `begin_object` checks:

```
remaining_tape_capacity >= projected_blocks × block_size + reserve_after_object
remaining_spool_bytes   >= spool_needed_after_object
```

Reserve includes: this object's filemark, pending completed sidecars, sidecars this object will generate, final partial epoch, parity_map/bootstrap overhead, safety margin. Per-sidecar size counts **full tape-file size** (replicated header/index `H+H`, parity shards `P`, footer, filemark).

### 9.10 Open items (carried into implementation)

1. **Incremental RS encoder byte-identical conformance** (impl step 11.6, "single hardest correctness gate"): incremental encoder byte-identical to batch encode AND to Appendix A's vectors.
2. **Power-loss validation** (steps 11.18a/b/c, 11.19): open-epoch rebuild, commit durability barrier, `TapeFileJournal` fsync / torn-trailing-record replay, and journal-only (no-database) full cycle on real hardware with deliberate power-loss cycle.
3. **MODE SELECT / MODE SENSE compression verification** (§11.6) on deployed LTO-9 drive models — exact page that works on HPE LTO-9 firmware.
4. **Large-object orchestrator policy** (§7.4 source, §5 source): whether upstream object splitting is firm policy or advisory per workload (mitigation for multi-TB single-object burst-rewrite cost).
5. **Bootstrap discovery scan cost tuning** (§15.1 source): content-driven placement is settled; remaining question is `BootstrapPlacementPolicy` aggressiveness for the akash object mix.

### 9.11 Error types

```rust
pub enum ParityError {
    UnrecoverablePendingEpoch       { failed_ordinal: u64, watermark: u64 },           // open epoch, no sidecar yet
    OutsideValidatedMapPrefix       { ordinal: u64, prefix_ordinals: u64 },             // block outside attested prefix
    ReconstructionIntegrityFailure  { failed_ordinal: u64 },                            // reconstructed block failed sidecar CRC
    RecoveryPlanExceedsMemoryBudget { needed_bytes: u64, max_recovery_cache_bytes: u64 },
    CapacityReserveExceeded         { cause: CapacityCause, /* ... */ },
    ObjectTooLargeForEmptyTape      { projected_blocks: u64, empty_tape_usable_blocks: u64, required_reserve_blocks: u64 },
    DriveCompressionEnabled,
    DriveCompressionModeUnknown,
    /* plus the usual: IoError, JournalError, ProtocolError, etc. */
}
```

**Recovery memory budget policy** (`BulkRecoveryPolicy`):

```rust
pub struct BulkRecoveryPolicy {
    pub max_recovery_cache_bytes: u64,      // default: min(8 GiB, 25% RAM)
    pub allow_windowed_recovery: bool,      // if true, multi-pass to fit budget; else fail
    pub max_stripes_per_window: u32,
}
```

---

## 10. Layer 4 — Local state

**Status: PARTIALLY IMPLEMENTED.** Crate `crates/remanence-state` contains the local SQLite index, audit log, lock, journal, and config surfaces. Remaining work is daemon integration, session orchestration, and any schema/API tightening needed by Layer 5.

### 10.1 Scope

Layer 4 owns the local state needed by the daemon across restarts:

1. Operator configuration and allowlists.
2. Append-only audit log.
3. Rebuildable local query index (SQLite) over 3c journals and tape catalogs.
4. Idempotency and operation/session projections derived from the audit log.
5. State-directory locking and migrations.

Layer 4 does NOT own:
- SCSI commands, drive positioning, media movement, hot-plug detection (those are Layer 1/2/3a).
- Tape body formats, tar layout, object chunking, file metadata encoding (Layer 3b).
- Parity encoding, bootstraps, sidecars, `TapeFileJournal` semantics (Layer 3c).
- gRPC, mTLS, request routing, session policy, cancellation policy (Layer 5).
- Cross-copy placement, retention policy, dedupe, scheduling, fleet databases (orchestrator).

Layer 4 may expose helpers Layer 5 uses, but it must not become an orchestrator.

### 10.2 Authority model

| State | Authority | Loss behavior |
|---|---|---|
| Operator config (`/etc/rem/config.toml`) | Authoritative | Daemon cannot safely start without valid config |
| Audit log (local append-only hash chain) | Authoritative for daemon-local actions and idempotency history | Tamper detectable; offsite checkpoints recommended |
| 3c tape-file journals (per-tape `FileTapeFileJournal`) | Authoritative for tape-file append/resume commit record | Rebuildable from tape (slower) |
| Tape-pool definitions and memberships | Operator config for v1; future audit-backed management requires explicit row authority | Reconcile from current config at daemon start; object-copy pool snapshots are not backfilled |
| SQLite query index | Derived projection | Delete and rebuild |
| Tape catalog cache | Derived from tape / 3b catalog | Delete and rebuild |
| Foreign archive catalog rows | Derived from source tape/dump via registered driver | Re-run `ArchiveReader::scan()` against each source; rebuild cost includes tape motion |

The SQLite index is **never a commit point.** It is a query accelerator. Losing it must not lose knowledge that cannot be rebuilt from the audit log, journals, config, or tape.

The 3c journal is also not a general catalog. It is deliberately narrow: the ordered committed tape files and parity watermarks needed for append/resume and recovery. Layer 4 indexes it for queries, but 3c remains generic over `TapeFileJournal`.

**Layer 4 reads 3c journals only through the 3c read-only replay surface** (`FileTapeFileJournal::open_shared_for_replay` plus `load_committed()`, or its successor). It must not reparse `.remjournal` bytes directly; on-disk framing, CRC rules, torn-record handling, and trusted-volume policy remain owned by Layer 3c.

### 10.3 On-disk layout

```
/etc/rem/config.toml

/var/lib/rem/
  state.lock
  audit/
    2026-05-25.remaudit
    2026-05-26.remaudit
  journals/
    <tape_uuid>.remjournal
  index/
    rem-state.sqlite
  cache/
    tapes/
      <tape_uuid>.cat
    aliases/
      <voltag>.json
```

State directories created `0700` by default. Layer 4 must not store long-term secrets; mTLS private keys and encryption keys belong to their own security design.

**State lock**: advisory exclusive `flock(LOCK_EX|LOCK_NB)` on `state.lock`. The file contents (`pid`, `host_id`, `started_at_utc`, `binary_version`) are diagnostic only — kernel lock release on process exit is the authority. If the lock cannot be acquired, startup returns `StateLockHeld` immediately. If the file exists but the lock can be acquired, Layer 4 overwrites the diagnostic contents and continues.

### 10.4 Config (`/etc/rem/config.toml`)

Minimum schema:

```toml
[daemon]
state_dir = "/var/lib/rem"
default_idle_timeout_seconds = 1800
read_only = false

[[libraries]]
serial = "7CBAD9CF74"
allow_derived_drive_identity = false

[[tape_pools]]
id = "camera.copy-a"
display_name = "Camera copy A"
copy_class = "copy-a"
content_class = "camera"

[[tape_pool_memberships]]
tape_uuid = "11111111-1111-1111-1111-111111111111"
pool_id = "camera.copy-a"

[journal]
dir = "/var/lib/rem/journals"
require_trusted_volume = true

[audit]
dir = "/var/lib/rem/audit"
fsync = true

[index]
sqlite_path = "/var/lib/rem/index/rem-state.sqlite"

[cache]
tape_catalog_dir = "/var/lib/rem/cache/tapes"
```

Validation: library serials unique; `allow_derived_drive_identity=true` only for an explicitly listed library; tape-pool ids unique and limited to ASCII letters, digits, `.`, `_`, `-`, and `:`; tape-pool memberships reference known pools and unique tape UUIDs; journal/audit dirs must not be tmpfs/network/untrusted-flush; if `read_only=true`, Layer 5 serves queries and refuses state-changing writes/moves/imports/exports/write-session-opens; unknown keys rejected until a migration explicitly accepts them.

Tape pools are daemon-local eligibility groups. `copy_class` and
`content_class` are optional opaque, fleet-coordinated hint strings for clients;
Remanence does not interpret them, and enforcement keys only on `id`/`pool_id`.
Sutradhara decides whether pool ids encode copy-wise
segregation (`copy-a`), content-wise segregation (`camera`), or both
(`camera.copy-a`). Static tape-to-pool membership can be supplied in config;
future management RPCs may add audit-log-backed membership changes.

Config reload not required for v0.1. Daemon reads config at startup.

### 10.5 Audit log

**Purpose:** durable record of daemon-local actions. Answers: who requested an operation; what the daemon attempted; what hardware/API result was observed; what idempotency key maps to which operation; what sessions were opened/closed/orphaned/failed/lost; what recovery warnings lower layers emitted.

#### 10.5.1 File format

One append-only file per UTC calendar day:

```
audit/YYYY-MM-DD.remaudit

fixed header:
  magic = b"REMAUD\x01"
  schema_version
  segment_date
  previous_segment_terminal_hash
  header_crc64

repeated records:
  offset  size   field
  0x00    4      u32le record_len
  0x04    N      canonical_cbor(AuditRecordWithoutRecordHash)
  0x04+N  32     record_hash
  0x24+N  8      u64le record_crc64
```

`record_hash = SHA256(previous_record_hash || canonical_cbor(AuditRecordWithoutRecordHash))`. The first record of a segment chains to `previous_segment_terminal_hash`. The first segment uses all zeroes.

On replay:
- Torn trailing record is ignored.
- CRC/hash failure before EOF is tamper/corruption — startup fails closed.
- Sequence gaps are corruption unless the segment header explicitly marks rotation.

#### 10.5.2 Audit record

```rust
pub struct AuditRecord {
    pub schema_version: u16,
    pub record_uuid: Uuid,
    pub sequence: u64,
    pub timestamp_utc: String,
    pub host_id: String,
    pub process_id: u32,
    pub actor: AuditActor,
    pub source_layer: SourceLayer,
    pub operation_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub idempotency_key: Option<Uuid>,
    pub event: AuditEvent,
    pub subject: AuditSubject,
    pub detail: BTreeMap<String, CborValue>,
}
```

Ordering is by `sequence`, not wall clock. Clock regression: emit `ClockRegressionObserved` and continue with monotonic sequence. Clock forward jump: emit `ClockForwardJumpObserved`; do not rewrite prior timestamps.

**Audit event variants** (minimum, stable enum tags required):

```
RequestReceived | OperationStarted | OperationProgress | OperationFinished | OperationFailed
CancelRequested | CancelledBeforeDispatch | CompletedAfterCancel | CancellationRejected | CompletionUnknown
SessionOpened | SessionCheckpointed | SessionClosed | SessionOrphaned | SessionLostByRestart
HardwareWarning | RecoveryEvent
ConfigLoaded | ConfigRejected
IndexRebuilt | ReadOnlyModeEntered | ReadOnlyModeLeft
AuditWriteFailed
```

`RequestReceived` projects to `operations.state='queued'` until a later
`OperationStarted` event moves the operation to `running`.

Layer 2 `LibraryAuditHook` events and Layer 3c recovery events must be representable without lossy stringly-typed conversion.

#### 10.5.3 Audit durability rules

- For state-changing operations: Layer 5 MUST append and fsync `OperationStarted` **before** dispatching the first irreversible CDB or write call.
- For completion: Layer 5 MUST append and fsync the terminal event after the lower layer returns. If that terminal append fails after an irreversible action has completed:
  1. Return an explicit audit-durability error to the client.
  2. Enter read-only/degraded mode.
  3. Require operator intervention or successful audit repair before accepting new state-changing work.

The data already committed to tape/journal remains valid. The degraded mode is about not continuing without an audit trail.

### 10.6 SQLite query index

Layer 4 owns an embedded SQLite file as a projection. Not a database server and not an authority.

Configuration:

```
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA foreign_keys=ON;
PRAGMA user_version=<schema_version>;
```

Implementation: `rusqlite` linked into the daemon; no external DB process allowed.

#### 10.6.1 Minimum tables

```sql
schema_meta(
  key text primary key,
  value blob not null
)

ingested_sources(
  source_kind text not null,       -- audit | tape_journal | tape_catalog
  source_id text not null,
  offset_bytes integer not null,
  terminal_hash blob,
  updated_at_utc text not null,
  primary key(source_kind, source_id)
)

tapes(
  tape_uuid blob primary key,
  voltag text,
  block_size integer,
  scheme_id text,
  data_blocks_per_stripe integer,
  parity_blocks_per_stripe integer,
  stripes_per_neighborhood integer,
  highest_protected_ordinal integer not null default 0,
  total_committed_ordinals integer not null default 0,
  last_committed_tape_file integer,
  state text not null,
  updated_at_utc text not null
)

tape_pools(
  pool_id text primary key,
  display_name text,
  copy_class text,
  content_class text,
  created_at_utc text not null
)

tape_pool_memberships(
  tape_uuid blob primary key,
  pool_id text not null,
  assigned_at_utc text not null,
  foreign key(pool_id) references tape_pools(pool_id)
)

tape_files(
  tape_uuid blob not null,
  tape_file_number integer not null,
  kind text not null,
  block_count integer not null,
  physical_start_hint integer,
  object_id text,
  first_parity_data_ordinal integer,
  epoch_id integer,
  protected_ordinal_start integer,
  protected_ordinal_end_exclusive integer,
  canonical_metadata_hash blob,
  bundle_uuid text,
  bundle_kind text,
  primary key(tape_uuid, tape_file_number)
)

objects(
  object_id text primary key,
  caller_object_id text,
  body_format text,
  logical_size_bytes integer,
  content_hash blob,
  metadata_hash blob,
  created_at_utc text not null
)

object_copies(
  object_id text not null,
  tape_uuid blob not null,
  tape_file_number integer not null,
  first_parity_data_ordinal integer,
  protected_until_ordinal integer,
  status text not null,
  pool_id text,
  primary key(object_id, tape_uuid, tape_file_number)
)

catalog_units(
  unit_id text primary key,
  tape_uuid blob not null,
  origin_kind text not null,       -- native_object | foreign_archive
  format_id text not null,
  native_object_id text,           -- set only for native_object
  scan_id text,                    -- set for foreign_archive scan units
  source_kind text,                -- set for foreign_archive refresh
  source_id text,                  -- trusted daemon-local source id/path token
  confidence text,
  entry_count integer,
  damage_event_count integer,
  last_scan_at_utc text,
  adapter_state blob,              -- daemon/driver-private, not public API
  created_at_utc text not null
)

idempotency_keys(
  actor_fingerprint text not null,
  idempotency_key text not null,
  request_fingerprint blob not null,
  operation_id text not null,
  terminal_state text,
  response_fingerprint blob,
  updated_at_utc text not null,
  primary key(actor_fingerprint, idempotency_key)
)

operations(
  operation_id text primary key,
  operation_kind text not null,
  state text not null,
  /* ... */
)

/* Plus a sessions table mirroring SessionOpened/Closed/Orphaned/Lost projections. */
```

`tape_pools` and `tape_pool_memberships` are projections of local
operator/orchestrator intent from config or audit replay, not on-tape authority.
A tape has at most one current pool assignment. When an object copy is
committed, `object_copies.pool_id` snapshots the tape's pool at that commit
point; this lets query clients distinguish data written under different
segregation intents even if the operator later changes the current assignment.
A tape with committed copies in a different or unknown pool must not be silently
reassigned into a new pool for future writes.

If startup fails with `TapePoolAssignmentConflict`, the operator should treat
the existing committed-copy pool snapshot as authoritative. Restore the prior
pool assignment, move the tape to a quarantine/read-only pool, or create a new
empty tape assignment for future writes. Do not edit the SQLite projection to
force reassignment; rebuilds must continue to derive pool state from config or
audited management records.

`catalog_units` is a thin cross-source projection over the native tables and
foreign scan records. Native rows reference `objects.object_id`; they do not
duplicate `object_copies` or `tape_files`. Foreign rows identify a scanned
archive unit and carry only driver-private `adapter_state` needed to resume or
refresh that scan. Per-foreign-entry SQLite rows are **not** required for v1:
`Catalog.ListEntriesInUnit` may re-run the registered driver's `scan()` and
return normalized entries on demand. If a later cache is added, it remains
delete-and-rebuild derived state and must not expose BRU/tar physical locator
columns as public schema.

`catalog_units.source_id` is a trusted daemon-local identifier. For v1 test and
operator-seeded dump flows it may be a local dump path, but it is not a
client-supplied path. Before any public RPC can create or update foreign archive
rows, the daemon must canonicalize the requested source and restrict it to a
configured foreign-source root allowlist.

### 10.7 Tape catalog cache

Per-tape file `cache/tapes/<tape_uuid>.cat` storing exact 3b catalog bytes plus a parsed CBOR projection for fast queries. Digest mismatch on read → discard cache, rebuild from tape.

### 10.8 Idempotency

Layer 5 supplies `idempotency_key` (UUID) in request metadata. Layer 4 stores `(actor_fingerprint, idempotency_key) → operation_id` plus request and response fingerprints. Replay rules:

- Same key + same request fingerprint → return original operation.
- Same key + different request fingerprint → reject with `IdempotencyConflict`.

Survives process restart by audit replay (no in-memory dependency).

### 10.9 Startup and replay

On startup:
1. Acquire state lock.
2. Load and validate config.
3. Open `FileAuditLog`; replay to populate operations / sessions / idempotency.
4. Open SQLite; if migration needed, migrate; if corrupted, move aside and rebuild; project configured tape-pool definitions.
5. For each known tape, open `FileTapeFileJournal` for shared replay; advance journal-ingestion offsets in `ingested_sources`; update `tapes`, `tape_files`, `object_copies` projections.
6. On journal contention with a live append session, mark ingestion pending and retry after the live session closes; do not take the exclusive append constructor just to build a projection.
7. Mark `SessionLostByRestart` for any session present in the audit log without a terminal close.

### 10.10 Public surface

Crate: `crates/remanence-state`.

```rust
pub struct RemConfig {
    pub daemon: DaemonConfig,
    pub libraries: Vec<LibraryConfig>,
    pub tape_pools: Vec<TapePoolConfig>,
    pub tape_pool_memberships: Vec<TapePoolMembershipConfig>,
    pub journal: JournalConfig,
    pub audit: AuditConfig,
    pub index: IndexConfig,
    pub cache: CacheConfig,
}

pub struct StatePaths {
    pub config_path: PathBuf,
    pub state_dir: PathBuf,
    pub audit_dir: PathBuf,
    pub journal_dir: PathBuf,
    pub sqlite_path: PathBuf,
    pub tape_cache_dir: PathBuf,
}

pub struct StateHandle {
    /* paths, config, lock, audit, index, idempotency — opaque to Layer 5 */
}

pub struct AuditReceipt {
    pub sequence: u64,
    pub record_uuid: Uuid,
    pub record_hash: [u8; 32],
    pub fsync_completed: bool,
}

impl StateHandle {
    pub fn open(paths: StatePaths) -> Result<Self, StateError>;
    pub fn config(&self) -> &RemConfig;
    pub fn audit(&mut self) -> &mut dyn AuditSink;
    pub fn catalog_index(&mut self) -> &mut CatalogIndex;
    pub fn idempotency(&mut self) -> &mut IdempotencyStore;
    pub fn journal_path(&self, tape_uuid: [u8; 16]) -> PathBuf;
    pub fn rebuild_index_from_journals(&mut self) -> Result<RebuildReport, StateError>;
}

pub trait AuditSink {
    fn append(&mut self, event: AuditEventRecord) -> Result<AuditReceipt, AuditError>;
}
```

`StateHandle` is `Send` but not `Sync`. Layer 5 obtains mutable state access through one owner task; read-only query snapshots may be exposed through typed methods that **do not leak raw SQLite connections.** Layer 5 must use typed query/update methods so that "derived, rebuildable, never authoritative" remains enforceable.

Also exposes: `LibraryAuditHook` adapter for Layer 2 events; 3c recovery/audit event adapter; JSON export for audit inspection; `rebuild-catalog-from-journals` command entry point for CLI/API.

### 10.11 Error model

```
ConfigInvalid | StateLockHeld | UntrustedStateVolume
AuditCorrupt | AuditTornTrailingRecord | AuditWriteFailed
DiskFull | JournalReplayFailed
IndexMigrationFailed | IndexCorrupt | IndexRebuildInProgress
IdempotencyConflict | CatalogCacheDigestMismatch
StateLockStale | ReadOnlyMode | PermissionDenied
```

Layer 5 maps these into gRPC status codes; Layer 4 must preserve structured causes.

### 10.12 Implementation plan

| Step | Description |
|---|---|
| 4.0 | Scaffold `crates/remanence-state`, errors, path handling, exclusive state lock. |
| 4.1 | Config loading + validation for `/etc/rem/config.toml` + test-path overrides. |
| 4.2 | `FileAuditLog`: header, framed records, hash chain, fsync, rotation, replay, JSON export. |
| 4.3 | SQLite migrations + typed `CatalogIndex` wrapper. |
| 4.4 | Journal ingestion through `FileTapeFileJournal::open_shared_for_replay` + `load_committed()`; per-tape lock contention retry; never hold replay handle across SQLite update tx. |
| 4.5 | Audit replay into `operations`, `sessions`, `idempotency_keys`. |
| 4.6 | Layer 2 + 3c audit adapters. |
| 4.7 | `rebuild-catalog-from-journals` + wipe/rebuild tests. |
| 4.8 | Startup-replay tests for crash windows (journal commit / SQLite update / terminal audit append). |
| 4.9 | No-database-process gate: tests pass with no Postgres/mysql service available. |

**Layer 5 should not start until 4.0–4.5 are usable.** Steps 4.6–4.9 can continue in parallel with the first Layer 5 skeleton if the public surface is stable.

Schema-version upgrades that add rebuildable projections, such as the v2
`catalog_units` table, require one `rebuild_from_authoritative_sources` pass
or equivalent journal re-ingest before queries for pre-upgrade tapes are
complete. SQLite migration creates the table; rebuild populates derived rows.
The v4 pool columns intentionally do not backfill historical
`object_copies.pool_id`; that field records the pool at copy commit time.
Operators that need historical pool labels for pre-v4 copies must run an
explicit rebuild/reclassification workflow outside the automatic migration.

### 10.13 Required tests (acceptance gates)

Unit / integration: 19 tests (1–19 in §13 source). Hardware-adjacent: 3 tests (run Layer 4 journal ingestion against QuadStor 3c live-test journals; wipe/rebuild after full QuadStor cycle; on deployed filesystem, forced power loss must not produce replay state beyond `fsync`-returned records). Power-loss gate deferrable until hardware qualification; harness designed now.

### 10.14 Acceptance criteria

Layer 4 v0.2 is complete when:

```
A. Daemon has one exclusive local state owner.
B. Config is validated; unsafe state/journal/audit locations rejected.
C. Audit records are hash-chained, fsync'd, replayable, tamper-evident.
D. Torn trailing audit record ignored; mid-log corruption fails closed.
E. SQLite is a rebuildable projection, not an authority.
F. Rebuilding SQLite from 3c journals produces equivalent queries.
G. Idempotency survives process restart by audit replay.
H. Layer 2 and 3c events recordable without lossy string conversion.
I. Crash windows around journal commit, index update, terminal audit have tests.
J. No hosted database process required or referenced.
```

Only after A–J should Layer 5 rely on Layer 4 for write/read session orchestration.
For schema bumps that add derived tables, this acceptance includes an explicit
operator-visible rebuild step so stale pre-upgrade indexes do not look
authoritative.

---

## 11. Layer 5 — gRPC API

**Status: PARTIALLY IMPLEMENTED.** Crate `crates/remanence-api` contains the
Daemon, Catalog, WriteSessionService, ReadSessionService, operations, and
LibraryService implementations. Crate `crates/remanence-daemon` serves the Unix
socket and mTLS TCP listeners. Authorization depth, audit-query RPCs, ranged
reads, and live library events remain. `proto/layer5.proto` is still draft and
not wire-stable.

### 11.1 Transport

gRPC over **HTTPS/2 with mutual TLS** (mTLS) authentication at the channel layer. Default port 8443. Plain TCP is not supported in production; an unauthenticated local-development mode (Unix socket only, no network listener) is available for testing.

Rationales:
- **Native streaming**: tape data paths are inherently streamed (multi-GB object writes, multi-GB byte-range reads, multi-hour verification with progress).
- **First-class cancellation requests**: see §11.5.
- **Strongly typed schemas**: `.proto` file is the source of truth; generated clients in any language.
- **Bidirectional event delivery**.

mTLS gives cryptographic authentication with no shared secret, per-client revocation, and integrates with operational best practices (short-lived certs, automated rotation via `step-ca` or equivalent).

A REST/JSON gateway can be added later via `grpc-gateway`; not a v1 deliverable.

### 11.2 Service model

Per `proto/layer5.proto`:

| Service | Purpose |
|---|---|
| `Daemon` | Health, version, operation lifecycle (`GetOperation`, `ListOperations`, `CancelOperation`, `WatchOperation`). |
| `LibraryService` | List libraries; inspect drives/slots/portals; refresh inventory; state-changing ops (`MoveMedium`, `LoadDrive`, `UnloadDrive`, `ImportElement`, `ExportElement`); `StreamLibraryEvents` server-stream for hot-plug bursts. |
| `Catalog` | The orchestrator's primary native-object read surface plus a cross-source discovery surface. `ListTapes` / `ListTapePools` expose pool assignment and eligibility metadata. `EnumerateObjects` remains the rebuildable-index hot path for native Remanence objects. `EnumerateUnits` / `ListEntriesInUnit` add a parallel view for native objects and foreign read-only archive units such as BRU or legacy tar. |
| `WriteSessionService` | `OpenWriteSession`, `AppendObject` (client-streaming), `CheckpointSession`, `CloseWriteSession`, `AbortWriteSession`, `GetWriteSession`. |
| `ReadSessionService` | `OpenReadSession`, `CloseReadSession`, `GetReadSession`, `ReadObjectRange` (server-streaming), `ReadFile` (server-streaming). |
| `Audit` | Read-only `QueryAudit` over the audit log. |

Idempotency: every state-changing RPC carries an `IdempotencyKey { value: bytes /* 16-byte UUID */ }`. Layer 4 persists the (actor_fingerprint, idempotency_key) pair; replay returns the original operation. Critical for tape operations.

Implementation status (2026-06-10): Layer 5 rejects non-empty idempotency keys
with `UNIMPLEMENTED` on RPCs whose per-request fingerprint and replay path are
not wired yet. This keeps retries from being silently accepted before the
contract above can be honored.

### 11.3 Long-running operations and cancellation

Tape operations span seconds to hours (LTO-9 first-load calibration alone can take up to two hours). All long-running operations are modeled as gRPC streams.

**`OperationState` variants** (per proto):

| State | Meaning |
|---|---|
| `QUEUED` | Operation registered but not yet dispatched. |
| `RUNNING` | In progress. |
| `SUCCEEDED` | Completed as requested. |
| `FAILED` | Completed with SCSI/transport error; no cancellation involvement. |
| `CANCELLED` | `CancelRequested` arrived and cancellation was honored. |
| `UNKNOWN` | `CompletionUnknown` — daemon lost contact with hardware after attempting cancel; physical state must be re-derived by rescan. **Do not treat as terminal — re-poll.** |

**Safe-point model:**
- A multi-object write session has a safe point between objects (commit boundary).
- A multi-chunk PFR read has a safe point between chunks.
- A single SCSI MOVE has only two safe points: before dispatch and after final status.
- A single `write_block` CDB has no in-CDB cancel.

Terminal-state vocabulary the daemon emits via the audit log (carried into `OperationStatus.error_summary` and full detail):

- `Succeeded` / `Failed` / `CancelledBeforeDispatch` / `CompletedAfterCancel` / `CompletionUnknown`.

(These map to the audit events of the same name; §10.5.2.)

`RequestReceived` projects as `QUEUED` until dispatch emits
`OperationStarted`. `CancellationRejected` is a transient cancellation event:
the operation remains `RUNNING` because it has passed the last safe cancellation
point or otherwise cannot honor the request. If useful to clients, the final
`OperationStatus.error_summary` may include a cancellation-rejected annotation,
but the operation's terminal state is still determined by the later completion
event.

**Resume after disconnect**: client reconnects with operation id; server replays missed progress events from a per-operation ring buffer. Reconnecting does NOT auto-cancel; the operation continues unless reconnect explicitly carries a `CancelRequested`.

### 11.4 The `Catalog.EnumerateObjects` streaming entry point

The single most important Layer 5 RPC for the orchestrator. Streams every object on a tape (or library, or all tapes) as `ObjectRecord`:

```protobuf
message ObjectRecord {
  bytes  object_id              = 1;  // Remanence UUID
  string caller_object_id       = 2;  // §4.2
  bytes  content_sha256         = 3;  // ← the logical identity Sutradhara keys on
  uint64 logical_size_bytes     = 4;
  string body_format            = 5;
  map<string, string> caller_metadata = 6;
  google.protobuf.Timestamp created_at = 7;
  repeated ObjectCopy copies    = 8;
}

message ObjectCopy {
  bytes  tape_uuid             = 1;
  uint64 tape_file_number      = 2;
  uint64 first_body_lba        = 3;
  google.protobuf.Timestamp last_verified_at = 4;
  ObjectCopy.Health health    = 5;
  string pool_id               = 6;  // empty means unassigned/unknown
}
```

With `reconcile_from_tape=true`, bypasses the SQLite cache and reads each tape's catalog directly — slow, intended for periodic scrub or after suspected drift.

This is the RPC that makes the orchestrator's rebuildable-index discipline possible.

### 11.4.1 Cross-source catalog units

Foreign tapes should be queryable by Remanence once a registered read driver can
scan them, but they must not pretend to have native Remanence object semantics.
Layer 5 therefore exposes a parallel **CatalogUnit** surface:

```protobuf
message CatalogUnit {
  bytes unit_id = 1;
  bytes tape_uuid = 2;
  string format_id = 3; // rem-tar-v1 | bru | tar-legacy | ...
  CatalogUnitOriginKind origin_kind = 4; // native_object | foreign_archive
  oneof origin {
    NativeUnitSummary native = 10;      // references ObjectRecord.object_id
    ForeignArchiveSummary foreign = 11; // scan_id, confidence, last_scan_at
  }
}
```

Rules:

1. `Catalog.EnumerateObjects`, `GetObject`, `FindObjectCopies`,
   `ListFilesInObject`, and `GetFile` remain the native Remanence object API.
   Sutradhara can keep using them for the normal write/read catalog path.
2. `Catalog.EnumerateUnits` is additive. It returns both native units and
   foreign archive units for operator discovery, migration, and recovery flows.
3. A native unit references an `ObjectRecord`; it does not duplicate native
   object/copy fields.
4. A foreign unit exposes normalized scan confidence and counts. Driver-private
   physical locators, BRU block positions, resync state, and similar details
   stay in opaque `adapter_state` inside the daemon/catalog cache.
5. `Catalog.ListEntriesInUnit` returns normalized entries: path, kind, optional
   size/mtime, state (`complete`, `partial`, `damaged`, `unsupported`,
   `unknown`), abstract integrity basis (`content_hash`,
   `format_checksum`, `parity_consistency`, `unknown`), and unattributed
   archive gaps when the driver had to skip source bytes that could not be
   assigned to one entry. It does not expose BRU-specific checksum names,
   physical record numbers, or adapter-private locator bytes as stable API
   fields.
6. Foreign per-entry persistence is optional derived cache. v1 may serve
   entries by re-running the driver's `ArchiveReader::scan()` against the
   source; a later cache must be deletable and rebuildable.
7. Migration/import is separate: scanning a BRU tape creates foreign catalog
   units; importing data writes new native `rem-tar-v1` objects. Any lineage
   back to the foreign source belongs in orchestrator state or caller metadata,
   not as a native object guarantee.

### 11.4.2 Tape pools

Tape pools are the local eligibility boundary for selecting cartridges. They
exist so Sutradhara can keep data separated by copy, content class, or both
without asking Remanence to own that policy.

Layer 5 exposes:

- `Catalog.ListTapePools` / `GetTapePool`: configured pool ids and optional
  `copy_class` / `content_class` hints.
- `Catalog.ListTapes(pool_id=...)`: all tapes in a requested pool.
- `Tape.pool_id`: the tape's current assignment.
- `ObjectCopy.pool_id`: the assignment snapshot when that copy committed.

`OpenWriteSession` accepts either a direct drive/tape target with an optional
`required_pool_id` guard, or a `TapePoolTarget` where the daemon chooses/mounts
an eligible tape from the requested pool. If no tape in the requested pool is
available, opening the session fails before any data is written.

Pool assignment is not on-tape authority. It is local operator/orchestrator
state projected from config or audit replay into Layer 4. The safety rule is
conservative: a tape with committed copies from a different or unknown pool must
not be silently reassigned into a new pool for future writes.
On `TapePoolAssignmentConflict`, restore the old assignment or retire/quarantine
that tape for writes; use a different empty tape for the new pool.

### 11.5 Sessions

Sessions are first-class server-side resources bound to one drive, one loaded tape, and one body format. The orchestrator opens a session, streams objects in / reads bytes out, then closes.

**Write session states**: `OPEN`, `CHECKPOINTED`, `CLOSED`, `ABORTED`, `ORPHANED` (dropped by daemon restart, recoverable), `LOST` (unrecoverable).

**Read session states**: `OPEN`, `CLOSED`, `LOST`.

Idle-timeout (configurable; default ~30 min of inactivity) auto-closes a forgotten session and records `ClosedByTimeout` in the audit log. Idle-close is a safety net, not a feature; orchestrators should call `CloseSession` explicitly.

**`OpenReadSession` fast-path**: if the requested tape is already loaded in a drive (e.g. left there by a prior session), rem skips MOVE+LOAD and returns READY immediately. Invisible optimization, never a slow path the caller didn't ask for.

**`OpenWriteSession` recovery**: pass `recover_session_id` (UUID of an orphaned session) instead of opening a fresh session. Returns the resumed `WriteSession` with current state.

`CloseSession` UNLOADs the drive and MOVEs the cartridge back to its home slot. **rem never keeps a tape loaded after `CloseSession`** — introducing eviction heuristics would force rem to invent policy it doesn't own.

### 11.6 Authorization

Derives from the client certificate. Roles encoded in the certificate subject:

- `orchestrator` — full access to write, read, library management.
- `operator` — admin access including drive cleaning, import/export, **not write operations**.
- `readonly` — query access to library state, tape contents, operation history.
- `admin` — additionally authorized for sensitive ops (read-only mode toggle, emergency stop).

Role-to-permission mapping explicit and enumerable. Authorization is scoped not just by role but by library: a certificate's subject may include a `libraries` attribute restricting the client to a specific set of library serials.

### 11.7 Reference client

The `rem` CLI is the reference Rust client — a thin wrapper over the gRPC service. A Python client library (`remanence-client`) generated from the `.proto` with hand-tuned ergonomics; both serve as integration references for orchestrators.

### 11.8 Open items in the proto draft

(Tracked in `/home/user/remanence/proto/README.md`):

1. Format-adapter message shapes: `AppendObjectStart.body_format_manifest` is currently `bytes`. Should this become a `oneof` per known format, or stay opaque? Type safety vs. format pluggability.
2. Multi-library routing: most RPCs take `library_uuid`. For single-library deployments this is noise. Consider a `DefaultLibrary` convention.
3. `OperationStatus.state == UNKNOWN` (CompletionUnknown): make sure clients are guided to re-poll, not to treat UNKNOWN as terminal.

Closed in the current implementation: `EnumerateObjects` / `EnumerateUnits`
server back-pressure uses bounded channel-backed streams over read-only SQLite
query handles, so the daemon does not materialize full catalog scans before
emitting the first response.

---

## 12. Cross-cutting

### 12.1 Persistence and authority — the unified picture

```
Authority (source of truth):
  Tape itself  >  3c per-tape journal  >  audit log  >  config
                                                         (separately, also authoritative)

Rebuildable from authority:
  SQLite query index   ← rebuildable from journals + audit
  Per-tape catalog cache  ← rebuildable from tape
  Library inventory snapshot  ← rebuildable from hardware
  Drive position / loaded tape  ← rebuildable from hardware
```

**Single load-bearing principle:** every cache must be deletable without data loss. The discipline is enforced by the typed Layer 4 surface (Layer 5 cannot reach raw SQLite, so it cannot accidentally make SQLite authoritative), by the journal being narrow (only commit/resume, not a general catalog), and by Layer 4's required `rebuild-from-journals` test (acceptance criterion F).

### 12.2 Hardware abstraction

#### 12.2.1 The drive enumeration problem

Kernel-assigned device nodes (`/dev/sg*`, `/dev/nst*` on Linux; `\\.\TapeN`, `\\.\ChangerN` on Windows) do not stably correspond to physical drive bays. After a reboot, HBA rescan, or drive replacement, the same physical drive may appear under a different device node. Software that hard-codes device paths breaks.

#### 12.2.2 The join-by-serial solution (empirically grounded)

Remanence never persists kernel device paths. The runtime mapping is derived fresh on each rediscovery — at process start, on explicit `LibraryService.Refresh` calls, and on hot-plug events.

Build the mapping by issuing two cheap queries:

1. **From the library**: READ ELEMENT STATUS with both `DVCID` and `CurData` bits set (CDB byte 6 = `0x03`), `element_type=4` (Data Transfer Elements). Response includes a 34-byte identifier descriptor per drive containing vendor, product, serial.
2. **From the kernel**: enumerate tape devices, issue INQUIRY + VPD page 0x80 to each, read the serial.

Join by serial number → runtime correspondence between library bay and host device.

> **Why both bits?** The primary discovery request sets DVCID and CurData together so the changer can return drive identifiers with cached element state in one read-only probe. This combination is verified against production MSL3040 firmware 3350 and QuadStor VTL fixtures. Discovery still probes the alternate CurData polarity as a compatibility fallback; older DVCID-alone notes predate the corrected CDB bit mapping and are not treated as hardware evidence.

#### 12.2.3 Fallback when DVCID is unavailable

For libraries not honoring DVCID+CurData (vendor firmware not yet tested — including the Overland XL80 — may behave differently):

1. Retry with `element_type=4` (drives-only) if the all-types form was used.
2. Retry with CurData inverted.
3. Final fallback: derive bay-to-serial from SCSI bus topology (drives on same `host:channel` as the changer, sorted by SCSI ID) and emit a discovery warning that the result depends on a vendor-specific convention.

Discovery is not failed by these fallbacks; the operator is informed via warnings in the discovery report.

#### 12.2.4 One model for partitioned and physically-separate libraries

Every deployment — single unpartitioned library; single chassis partitioned into N logical libraries; M physically separate libraries; any mix — reduces to **a flat list of logical libraries**. Operations are gated on explicit per-library opt-in: the daemon refuses state-changing commands against any library not on its allowlist.

### 12.3 Security

#### 12.3.1 Threat model

**In scope:** network-based attackers; compromised credentials of legitimate clients; insider threats; ransomware reaching the daemon host; tampering with the audit log.

**Out of scope:** physical access to the library; root-level compromise of the daemon host (assumed game-over); supply chain attacks against the build toolchain; side-channel attacks against the host hardware.

#### 12.3.2 Defense in depth

- **Cryptographic authentication**: mTLS for every API client.
- **Least privilege**: daemon runs as unprivileged user with only the OS capabilities and device permissions it strictly needs.
- **Process hardening**: systemd unit applies `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`, `RestrictAddressFamilies`, and related options.
- **Library allowlist**: explicit list of library serials the daemon is allowed to issue state-changing commands against.
- **Append-only audit log**: hash-chained, periodically checkpointed offsite. Tampering detectable. Audit log is local state (§10.5), not a database.
- **Anomaly thresholds**: destructive operations beyond normal rates require explicit override.
- **No destructive primitives**: no API endpoint destroys data. Tapes can be retired, exported, and scratched only by deliberate operator action with explicit confirmation.
- **Read-only and emergency stop modes**: a single privileged operation puts the daemon into a state where it serves reads and refuses writes/moves.
- **Tape-content recovery without daemon state**: every tape self-describes (§12.1), so losing the daemon's local cache does not lose tape contents — a fresh daemon mounts the tape and rebuilds the cache. Audit-log checkpoints are shipped offsite for tamper detection, not for tape-content recovery.

#### 12.3.3 Certificate management

Issued by a private CA managed via `step-ca` or equivalent. Short certificate lifetimes (90 days); automated renewal; revocation via CRL or OCSP. The CA itself is offline or air-gapped from the daemon host.

#### 12.3.4 Tape-content encryption

Per-tape encryption using LTO hardware encryption (AES-256-GCM) via the SCSI SECURITY PROTOCOL IN/OUT commands. The drive does the encryption; rem stores wrapped-DEK references in the bootstrap / catalog cache (never the DEK itself). Key management is integrated via a `KeyProvider` trait with implementations for static-key (dev), file-backed (small deployments), and external KMS (production). Encrypted-tape interactions with PFR are documented in `pfr-reference.md` §6 (per-block GCM requires `chunk == block`).

### 12.4 Audit and observability

The hash-chained audit log (§10.5) is the canonical record of daemon-local actions. It is grep-able by design (newline-delimited CBOR per record; JSON export available via `rem audit export-json`).

Structured tracing via the `tracing` crate (no separate logging system). Trace events at INFO level cover request start/end and significant state changes; DEBUG covers per-CDB issuance.

No built-in metrics export at v0.4 (Prometheus / OpenTelemetry are deferrable). If query-by-time-range on the audit log becomes a felt need, SQLite is the escape hatch — but it would index the audit log, not replace it.

### 12.5 Error model conventions

- **Layer 1** returns `ScsiError` with `CheckCondition(sense_key, asc, ascq)` or `Transport(io_error)`. Sense data is preserved structurally, never stringified at the error boundary.
- **Layer 2** (`LibraryHandle::is_dirty()`, `DirtyCause`) treats `Transport` errors from in-flight state-changing CDBs as `CompletionUnknown` and marks the snapshot dirty with a precise cause.
- **Layer 3a** (`TapeIoError`) preserves Layer 2's dirty-state vocabulary; transport errors propagate dirty-state via a `pub(crate)` helper.
- **Layer 3c** (`ParityError`) adds structured recovery-failure variants (`UnrecoverablePendingEpoch`, `OutsideValidatedMapPrefix`, `ReconstructionIntegrityFailure`, `RecoveryPlanExceedsMemoryBudget`, `CapacityReserveExceeded`, `ObjectTooLargeForEmptyTape`).
- **Layer 4** (`StateError`) covers config/audit/journal/index/idempotency variants; preserves structured causes.
- **Layer 5** maps each layer's error to a gRPC status code with structured detail in the trailer. `OperationState::UNKNOWN` is the unified "we don't know if it completed" surface.

**Universal rule:** never silently downgrade `CompletionUnknown` to either success or failure. The user / orchestrator must be told the truth and given a way to reconcile.

---

## 13. Open items per layer

(Consolidated from each layer's open-questions list. Items closed in v0.7.2 / v0.2 addenda are not listed.)

**Layer 1:** none load-bearing. Missing fixtures (VPD 0x83, sg_logs `-a`, page 0x17, page 0x2E) for next MSL3040 access window.

**Layer 2:** Layer 2c's daemon-side integration (consume burst stream → `refresh()` policy) belongs to Layer 5 and is therefore still open until Layer 5 lands.

**Layer 3a:** step 9.9 — production MSL3040 live smoke + fixture capture on a scratch LTO-9 tape. Pending hardware window.

**Layer 3b:** entire layer unimplemented; design ready. Mock-`BlockSink`/`BlockSource` development can begin today.

**Layer 3c (§9.10):**
1. Incremental RS conformance (impl step 11.6 — single hardest correctness gate).
2. Power-loss validation (steps 11.18a/b/c, 11.19) on real hardware.
3. MODE SELECT / MODE SENSE compression page on deployed LTO-9 models.
4. Large-object orchestrator policy (upstream object splitting firm vs. advisory).
5. Bootstrap discovery scan-cost tuning for akash workload.

**Layer 4 (§10.15 / §15 source):**
1. Stability of JSON audit export — debug convenience until v1 is the recommended posture.
2. Tape catalog cache format — exact 3b bytes plus parsed projection (recommended).
3. SQLite split — one file for v0.2 with clear table ownership (recommended).
4. Terminal audit failure after successful irreversible action: hard error plus read-only/degraded mode (recommended), because audit durability is a product promise.

**Layer 5 (§11.8):** 4 proto-draft items (format-adapter shapes, multi-library routing, streaming back-pressure, `UNKNOWN`-state client guidance).

**Cross-cutting:** none load-bearing at this revision.

---

## 14. Roadmap and sequencing

### 14.1 Critical path to "orchestrator can write and read real tapes via Layer 5"

In order, each step independently useful:

1. **Layer 3b implementation** (`remanence-format`, `rem-tar-v1` writer + reader against mock `BlockSink`/`BlockSource`). Spec ready; can begin immediately. Likely scaffolded as a codex-driven implementer cron similar to the 3c implementer.
2. **Layer 4 steps 4.0–4.5** in parallel with 3b: state crate scaffold, config, audit log, SQLite migrations, journal ingestion, audit replay.
3. **Layer 3b integration with Layer 3c** on QuadStor: actual `ParitySink` + `rem-tar-v1` writer round-trip.
4. **Layer 3c remaining hardening** (§9.10 items 1, 2, 3) — these can proceed in parallel with 3b via the existing codex implementer cron.
5. **Layer 5 skeleton** once Layer 4 4.0–4.5 are usable: gRPC server, mTLS, the Catalog read surface first (just `Catalog.EnumerateObjects` against the SQLite index), then the WriteSession / ReadSession surface.
6. **Layer 5 + Layer 2c integration**: the daemon consumes hot-plug bursts and invokes `LibraryHandle::refresh()` on the right subset.
7. **Layer 4 steps 4.6–4.9** in parallel with Layer 5 work.
8. **Step 9.9** (Layer 3a live MSL3040 smoke) at the next hardware window — opportunistic, not blocking the above.
9. **Sutradhara week-1 vertical slice** (separate project) can develop against a stub Layer 5 client throughout this period; integration happens after step 5.

### 14.2 Legacy-format readers (post-MVP)

After the core path lands, add reader-only crates for the d2 migration:

- `remanence-tar-legacy` — standard pax-tar tapes (block size 512). Wraps Rust `tar` crate.
- `remanence-bru` — reverse-engineered BRU/BRU-PE reader.

Both expose the normalized format-driver reader surface so Sutradhara sees one uniform interface. They are foreign read-only drivers: they decode physical tape records or dump bytes first, then emit catalog and restore events. Implementation order: `remanence-tar-legacy` first (smaller scope, immediate value), `remanence-bru` second (research effort, blocks only the oldest tapes).

### 14.3 What we are NOT building at v0.4

- Dynamic format plugins (binary plugin loader — see §8.5).
- Multi-volume tar extensions or cross-tape file spans (§5.6 source).
- Format conversion (`rem convert-format`) — orchestrator's job.
- Tape-level deduplication.
- Cross-tape search.
- WORM enforcement beyond hardware-provided.
- macOS port.

---

## 15. Glossary

| Term | Definition |
|---|---|
| **BodyLba** | Per-object logical block address starting at 0 for each new object. Paired with `tape_file_number` to form a complete address. The space `rem-tar-v1` operates in. |
| **Bootstrap** | A Layer 3c-written tape file (at BOT, content-driven mid-tape positions, and at finish) carrying the parity scheme, tape UUID, sequence, and filemark-map digest. Catalog-less reader rebuilds map from highest-sequence valid bootstrap. |
| **Caller-object-id** | An opaque string the orchestrator supplies on write; preserved by Remanence on tape and in queries. The integration seam (§4.2). |
| **CommittedBundle** | The atomic journal unit. Kinds: Object, Control, ResumeSidecars, Finish. |
| **DriveHandle** | Layer 2/3a per-drive transport handle; owns the SCSI device and (since Layer 3a in-crate decision) the dirty-state pointer. |
| **DVCID** | Drive Identifier bit 0 in READ ELEMENT STATUS CDB byte 6, used with CurData bit 1 by the primary discovery probe to request drive serials in the response. |
| **Filemark** | A SCSI tape filemark separating tape files on tape; written by Layer 3c at object close. |
| **FileTapeFileJournal** | Default `TapeFileJournal` impl: one append-only file per tape, `std` + CBOR + CRC-64, fsync = commit. |
| **LibraryHandle** | Layer 2 per-library handle; owns the changer transport and the dirty-state snapshot. |
| **Object** | One Remanence archival unit. One object = one pax tar archive on tape (for rem-tar-v1). Has a `caller_object_id` (orchestrator) and an `object_id` (Remanence UUID). |
| **ParityDataOrdinal** | 3c-internal logical sequence numbering only protected data records across object archives, skipping filemarks. RS epochs are defined over this space. |
| **ParitySink** | Layer 3c's write-side handle; wraps `RawTapeSink` and a `TapeFileJournal`, exposes `begin_object` / `finish_object` / `checkpoint` / `finish`. |
| **ParitySource / ObjectParitySource** | Layer 3c's read-side handles; `ObjectParitySource` exposes `BlockSource` for one object plus `recover_block_at`. |
| **PFR** | Partial File Restore — byte-range read within a file. |
| **rem-tar-v1** | The default body format. Constrained pax tar subset with chunk alignment for PFR; per-file content hashes in pax headers; CBOR manifest at end of each archive. |
| **SidecarEpochDirectory** | Per-tape directory of completed parity epochs, carried in the bootstrap if it fits or spilled to an external `parity_map` tape file. |
| **TapeFileJournal** | Layer 3c persistence trait; commits bundle by fsync. |
| **Voltag** | Operator-visible cartridge barcode label. Non-authoritative — `tape_uuid` is the Remanence identity. |

---

## 16. Document lineage and what this supersedes

### 16.1 Predecessor documents

This v0.4 consolidates and supersedes:

| Predecessor | Status after v0.4 lands | Recommendation |
|---|---|---|
| `spec-v0.3.md` (May 2026) | Superseded | Mark as historical; delete after one release cycle. |
| `spec-v0.2.md` | Already historical | Delete. |
| `spec-v0.1.docx` | Already historical | Delete. |
| `rem-tar-v1-design.md` (v0.9.3) | Superseded; content absorbed in §8 (and reconciled to 3c v0.7.2 in §8.7.12). | Mark as historical; delete after one release cycle. |
| `layer3c-design.md` (v0.7.2) | Superseded; content absorbed in §9. | Mark as historical; delete after one release cycle. |
| `layer3c-design-v0.7.2.md` | Duplicate of `layer3c-design.md` | Delete now. |
| `layer3c-design-v0.6.md` | Already historical | Delete. |
| `layer3c-design-v0.5.md` | Already historical | Delete. |
| `layer3c-design-v0.2.md` | Already historical | Delete. |
| `layer3c-epoch-revision.md` | Already marked superseded (folded into 3c v0.3.1) | Delete. |
| `remanence-3c-implementation-addendum-v0.2.md` | Superseded — explicitly folded into 3c v0.5 per `layer3c-design.md` v0.6 changelog. | Delete. |
| `layer4-implementation-addendum-v0.2.md` | Superseded; content absorbed in §10. | Mark as historical; delete after one release cycle. |
| `layer4-implementation-addendum-v0.1.md` | Already marked superseded by v0.2 | Delete. |
| `layer3a-design.md` | Superseded; content absorbed in §7. | Mark as historical; delete after one release cycle. |
| `layer3b-design.md` | Superseded; content absorbed in §8. | Mark as historical; delete after one release cycle. |
| `layer2-design.md` (2a) | Superseded; content absorbed in §6.1. | Mark as historical; delete after one release cycle. |
| `layer2-design-feedback.md` | Historical review notes | Delete. |
| `layer2b-design.md` | Superseded; content absorbed in §6.2. | Mark as historical; delete after one release cycle. |
| `layer2b-design-feedback.md` | Historical review notes | Delete. |
| `layer2c-design.md` | Superseded; content absorbed in §6.3. | Mark as historical; delete after one release cycle. |
| `3b-catalog-schema-followup.md` | Superseded; catalog schema content absorbed in §8.7 and §9.6. | Mark as historical; delete after one release cycle. |
| `remanence-testing-plan.md` | Cross-layer testing plan; **retain** as a separate operational doc. | Keep. |
| `pfr-reference.md` | Detailed PFR mechanics + worked latency examples; **retain** as a separate reference doc. | Keep. |
| `why-remanence.md` | Positioning piece; **retain** as separate marketing/orientation doc. | Keep. |
| `INSTALL.md` | Operator runbook; **retain**. | Keep. |
| `README.md` | Project readme; **update** to point at this consolidated spec. | Update. |
| `JOURNAL.archive.md` | Legacy prose journal; frozen 2026-05-18. | Keep as historical. |
| `journal/YYYY-MM-DD.json` | Dated session notes; keep — these are the operational record. | Keep. |

### 16.2 Suggested cleanup sequence (operator action)

When you're satisfied with v0.4:

1. **Immediate deletions** (true duplicates / explicit prior supersedes): `layer3c-design-v0.7.2.md`, `layer3c-design-v0.6.md`, `layer3c-design-v0.5.md`, `layer3c-design-v0.2.md`, `layer3c-epoch-revision.md`, `remanence-3c-implementation-addendum-v0.2.md`, `layer4-implementation-addendum-v0.1.md`, `layer2-design-feedback.md`, `layer2b-design-feedback.md`, `spec-v0.2.md`, `spec-v0.1.docx`.
2. **Update README.md** to point at this spec; remove the long list of layer-specific design files; keep pointers to `pfr-reference.md`, `remanence-testing-plan.md`, `INSTALL.md`, `why-remanence.md`, and the journal.
3. **After one release cycle** with no missing-context surprises, delete the larger superseded docs: `spec-v0.3.md`, `rem-tar-v1-design.md`, `layer3c-design.md`, `layer4-implementation-addendum-v0.2.md`, `layer3a-design.md`, `layer3b-design.md`, `layer2-design.md`, `layer2b-design.md`, `layer2c-design.md`, `3b-catalog-schema-followup.md`.

### 16.3 Change discipline going forward

- This spec is the single source of truth. Future contracts changes land here, not in new layer-specific docs.
- Implementation addenda (e.g. an eventual Layer 4 implementation report) are short patches against this spec, not standalone documents.
- Versioning: v0.5, v0.6, ... track substantive consolidated revisions; v1.0 marks the first "API stable, format stable" line.
- The journal (`journal/YYYY-MM-DD.json`) remains the dated operational record of work-in-progress; this spec is the steady-state contract.

---

**End of spec-v0.4.**
