# Remanence — Tape Library Management Tool for LTO Archives

**Version 0.3 (Draft) — May 2026**
**Archives Team**

> Supersedes `docs/spec-v0.2.md` (May 2026). v0.2 is retained as a
> historical artifact. v0.3 records six foundational decisions taken
> in conversation on 2026-05-18:
>
> 1. Remanence is a **minimal mechanism** — "filesystem for tape" — not
>    an archival or orchestration system.
> 2. The architecture is **OS-portable by construction**. Linux is the
>    primary supported target; Windows is a portability goal; macOS is
>    explicitly out of scope.
> 3. There is **no standalone database**. Authoritative tape state lives
>    on the tape; the daemon keeps a regenerable disk cache and an
>    append-only audit log. PostgreSQL is removed from the architecture.
> 4. The **on-tape body format is pluggable** via a capability-tiered
>    trait system (`TapeFormat`, `FileAddressable`, `ByteRangeAddressable`,
>    plus orthogonal capabilities). Pax-tar is one option; `rem-chunked-v1`
>    is the new default that supports all tiers.
> 5. **Single-file and byte-range restore (PFR)** are first-class
>    requirements, met via LBA-indexed chunked containers and the
>    `ByteRangeAddressable` capability.
> 6. The **external API moves from REST + SSE to gRPC** with bidirectional
>    streaming. An optional REST gateway can be added later. The CLI
>    becomes a thin client over the gRPC interface.
>
> All other v0.2 content (operating context, hardware abstraction,
> security model, key management) carries forward with edits where
> the six decisions above touch them.

---

## 1. Introduction

### 1.1 Purpose

Remanence is the **tape-mechanics layer** for LTO-based archives. It exposes a
narrow, well-documented interface for higher-level archive management systems
to load, write, read, locate, and account for data on tape. It is to a tape
library what a filesystem driver is to a block device: a faithful mechanism,
not a policy engine.

The name refers to magnetic remanence: the property of a ferromagnetic
material to retain its magnetisation after the external magnetising field is
removed. It is the physical reason data persists on tape when power is
disconnected, and the reason tape remains the standard medium for long-term
archival storage. The command-line interface is named `rem`.

### 1.2 Why this exists

This project was built for a multi-petabyte tape archive that has historically
been managed by an in-house system (dwara v2). A migration to a commercial
system (Atempo Miria) has been in progress for an extended period without
successful production deployment. Remanence is being developed as a focused,
well-engineered alternative to the tape-management portion of that stack —
buildable by a small team, defensible on technical merits, easy to integrate
with any orchestration layer, and deliberately uninterested in the workflows
above it.

The design priorities, in order:

1. **Correctness and data integrity above all.** This system writes the only
   copies of irreplaceable data.
2. **Minimal scope.** Anything that is not a tape mechanism is out of scope.
   If three orchestrators ask for the same higher-level feature, that's a
   signal — not before.
3. **Operational legibility.** An operator with the documentation should be
   able to understand what the system is doing and why.
4. **Format longevity.** Data written today must be readable in thirty years,
   ideally with standard Unix tools, even if Remanence itself no longer
   exists.
5. **Ease of integration.** Higher-level systems should find Remanence
   pleasant to integrate with.
6. **Security against realistic threats.** The tool must not be a soft target
   for attackers seeking to disrupt or damage the archive.

### 1.3 What this document is

A high-level specification of architecture and key design decisions. It is
not implementation-ready. It captures the choices that have been made and
the reasoning behind them so that subsequent detailed design can proceed
from a stable foundation. Layer 2 discovery and state-changing operations
are drafted in `docs/layer2-design.md` and `docs/layer2b-design.md` and
already partially implemented; tape format details are governed by the
capability-tier system specified in §5.

### 1.4 Scope boundaries

The filesystem analogy is the decision rule. A filesystem driver does not
manage backup retention, does not maintain a cross-volume search index,
does not schedule jobs, and does not authenticate users. It exposes
mechanism. Remanence does the same.

**Remanence owns:**

- The state of every tape library partition the host can reach (drives,
  slots, mailslot, picker).
- Tape-level operations: load, unload, write, read, locate, seek, eject,
  import, export, verify.
- Single-file and byte-range restore (PFR) via the on-tape format
  capability system (§5).
- Tape-level metadata: per-tape state, MAM, health, write history.
- The on-tape catalog format (rem's bookkeeping at file mark 0 of each
  tape; not pluggable).
- The on-tape body format **trait surface**, plus a small set of
  built-in implementations. New formats are added by external crates
  implementing the trait.
- Hardware health, diagnostics, drive cleaning workflows.
- Optional encryption for tape contents, including key-management
  integration.

**Remanence does not own:**

- The logical file catalog across all storage tiers.
- Multi-tier orchestration (disk, cloud, tape coordination).
- User-facing archive workflows (ingest, retrieval policy, retention).
- Disk or cloud storage backends.
- End-user authentication (above the API authn layer).
- Cross-tape search ("which tapes contain X?"). Remanence exposes the
  per-tape catalogs; the orchestrator builds search indexes if it
  wants them.
- Job scheduling, retention policy, deduplication.
- WORM enforcement beyond what the hardware itself provides.

These are the responsibility of the orchestration layer (Dwara v3 or any
equivalent system). Remanence exposes its services via an API; the
orchestration layer composes them into archive workflows.

---

## 2. Operating Context

### 2.1 Hardware

Remanence is designed against a heterogeneous fleet of LTO libraries.
Primary production targets and their drive generations:

| Library | Drive generations | Notes |
|--|--|--|
| **HPE StoreEver MSL3040** | LTO-9 (primary), LTO-7 | Single chassis (expandable to seven stacked modules / 280 slots). Routinely partitioned into multiple logical libraries — each partition appears as an independent SCSI medium changer (see §6.5). |
| **Overland Storage XL80** | LTO-7, LTO-6 | Legacy library kept in service to broaden the addressable archive fleet. Multiple drive bays; same SCSI command set as the MSL3040 modulo vendor quirks. |

Supported LTO drive generations across the fleet: **LTO-6, LTO-7, LTO-8,
LTO-9**.

Drive count and slot count are configurable per deployment. The tool must
scale to a fully populated multi-chassis configuration and to a host with
multiple libraries attached simultaneously (a single Remanence daemon can
manage all of them).

### 2.2 Host environment and OS portability

**Primary supported OS: Linux** (Ubuntu 24.04 LTS reference, RHEL 9, Rocky 9,
AlmaLinux 9 with kernel 5.10 or later). The daemon runs as a long-lived
systemd service with appropriate Linux capabilities for SCSI generic device
access. CentOS 7 and other end-of-life distributions are explicitly not
supported, particularly because LTO-9 first-load calibration interacts
poorly with older kernel SCSI driver behavior.

**OS portability is a design constraint.** The codebase is structured so
that operating-system-specific code is confined to clearly named seams,
behind traits that have at least one Linux implementation in tree. This
discipline keeps a future Windows port small and self-contained:

| Seam | Linux today | Other OSes |
|--|--|--|
| SCSI passthrough | `LinuxSgTransport` (SG_IO ioctl) | Windows: SPTI via `DeviceIoControl(IOCTL_SCSI_PASS_THROUGH_DIRECT)`. FreeBSD: CAM. macOS: not supported. |
| Device enumeration | Linux sysfs walker | Windows: `SetupDi*`. Trait surface: `DeviceEnumerator`. |
| Device path type | `DevicePath(OsString)` newtype | Linux uses `/dev/sg0`, `/dev/nst0`. Windows uses `\\.\Tape0`, `\\.\Changer0`. |
| Privilege model | tape group + `CAP_SYS_RAWIO` | Windows: admin or `SeManageVolumePrivilege`. Surfaced via a `PrivilegeAdvice` trait so CLI hints are not Linux-only. |
| Hot-plug notification | udev | Windows: `RegisterDeviceNotification`. |

**Windows** is a portability goal — not a v1 deliverable, but the
architectural seams above must remain clean as new layers land, and
nothing Linux-specific is allowed to leak into types or error messages
of layers 2 and above. An outside contributor implementing the Windows
transports should be able to do so without touching the upper layers.

**macOS** is explicitly out of scope. There are essentially no SAS HBAs
in the Apple ecosystem; LTO tape on Mac is not a realistic deployment.
We may accept patches that compile-cleanly on Darwin but will not test
them.

The daemon does not require a container or VM, and none is recommended
for production: hardware-managing daemons benefit from direct access to
kernel SCSI and hot-plug subsystems without an intervening abstraction.

### 2.3 Integration target

The primary integration target is Dwara v3, an in-development Python-based
archive management system that owns the cross-tier catalog. Remanence
exposes its functionality via a gRPC service over TLS (§7); Dwara v3 is a
client of that service. The service is designed to be usable by any
client, not specific to Dwara v3.

A reference Python client library (generated from the `.proto` schema) is
provided, with a target ergonomics of "common operations expressible in
under ten lines of code."

### 2.4 Coexistence with other tape-handling software

Remanence will routinely operate on hosts where other software *also* talks
to the same SCSI hardware — either a different Remanence-managed partition,
or a different tape-management tool entirely (for example, dwara v2 managing
the LTO-7 partition of the production MSL3040 chassis). Discovery returns a
complete view of every reachable library and partition, but operations are
gated on explicit per-partition opt-in (see §6.5 and §8.2). **Remanence
never operates on a partition it has not been told it owns.**

---

## 3. Architecture

### 3.1 Process model

Remanence runs as a single long-lived daemon process on a dedicated host.
**A single daemon manages every logical library the host can reach and the
operator has configured it to own.** A logical library is the operational
unit: one SCSI medium changer with its own drives, slots, and import/export
ports. Whether two logical libraries happen to share a physical chassis is
irrelevant to Remanence — there are no SCSI operations that cross logical-
library boundaries, and the daemon treats each one independently. The same
uniformity applies regardless of vendor and regardless of how the
underlying hardware exposes itself.

The daemon exposes a gRPC service for clients and persists a small amount
of local state to disk (§3.3). There is no database server in the
architecture.

The daemon is built in Rust. The choice is driven by: memory safety in code
that constructs and parses binary SCSI structures; clean integration with
Linux kernel subsystems (sg, udev) via mature crates; suitability for
long-running concurrent operations via the tokio async runtime; single
static-binary deployment with minimal runtime dependencies; and ease of
exposing trait-based extension points to external format crates.

### 3.2 Layering

The codebase is structured in clear layers, with each layer testable
independently:

- **Layer 1: SCSI core** (`crates/remanence-scsi`). Pure SCSI command
  construction, ioctl invocation, and response parsing. INQUIRY, VPD pages,
  READ ELEMENT STATUS, MOVE MEDIUM, INITIALIZE ELEMENT STATUS, PREVENT/ALLOW,
  LOG SENSE, READ/WRITE ATTRIBUTE, SECURITY PROTOCOL IN/OUT, and tape-side
  commands (LOAD, UNLOAD, REWIND, LOCATE, READ POSITION, SPACE, WRITE
  FILEMARKS, READ, WRITE). No business logic, no I/O policy, no concurrency.
  Testable in isolation against captured response fixtures.
  **Status: largely landed for the SMC-3 side; tape-side commands pending.**
- **Layer 2: Library runtime model** (`crates/remanence-library`). The
  in-memory model of library topology, drive states, and tape inventory.
  Performs the join-by-serial logic mapping library-reported drive bays to
  host-side device nodes (§6). Split into three sub-layers:
  - **2a — Discovery + identity model.** Cold enumeration, join-by-serial,
    library/drive/slot value types. **Status: complete; live-tested on
    QuadStor + production MSL3040.**
  - **2b — State-changing operations.** Move, load, unload, import,
    export, with phase-aware error reporting and a dirty-state machine
    for completion-uncertain failures. **Status: complete; live-tested
    on QuadStor + production MSL3040.**
  - **2c — Hot-plug watcher.** Subscribes to OS hot-plug events (Linux:
    udev; Windows: `RegisterDeviceNotification`) and emits coalesced
    notification bursts that the daemon can consume to trigger
    re-discovery. The watcher is notification-only; it does not call
    SCSI or hold a `LibraryHandle`. **Status: implementation complete
    and live-tested on akash 2026-05-18. Linux backend ships behind
    the `linux-udev` Cargo feature; daemon-side integration with
    `LibraryHandle::refresh()` is the orchestrator's job and is not
    in this crate.**
- **Layer 3a: Tape mechanism** (`crates/remanence-library`, new
  `tape_io` module — see `docs/layer3a-design.md` §2 for why this
  isn't a separate crate as earlier drafts proposed). The
  mechanism-only vocabulary of tape-side ops: LOAD/UNLOAD,
  LOCATE-by-LBA, SPACE, WRITE FILEMARKS, raw block READ/WRITE,
  READ POSITION. No format awareness whatsoever — this layer
  pushes and pulls blocks at LBAs. Streaming, buffering, rate
  matching, and progress reporting live here. No request queue;
  one LOCATE + read per call. Batching, prefetch, and ordering
  decisions live in the caller (Layer 5 → orchestrator), not
  here.
- **Layer 3b: Tape format** (`crates/remanence-format`). The `TapeFormat`
  capability-tier trait system (§5), plus built-in implementations
  (`rem-chunked-v1`, `pax-tar-v1`), the on-tape catalog reader/writer (rem's
  bookkeeping, not pluggable), and the format registry. External crates
  (e.g. `rem-format-bacula`) implement the trait to add new formats. This
  layer talks to Layer 3a for the block-level I/O.
- **Layer 4: Local state** (`crates/remanence-state`). On-disk persistence
  for the small amount of state the daemon must keep across restarts: the
  append-only audit log; per-tape catalog cache files (regenerable from
  tape); the library allowlist and configuration. **No database server.**
  See §3.3.
- **Layer 5: API** (`crates/remanence-api`). gRPC server with mTLS
  authentication. Translates external requests into Layer 3 operations and
  Layer 4 state queries. Enforces authorization and rate limiting. Emits
  structured audit entries for every request. The `rem` CLI is a thin
  client over the same gRPC service.

### 3.3 Persistence

**There is no database.** The authoritative record of what is on a tape is
the tape itself — every cartridge contains a self-describing catalog
written at file mark 0 (§5.8) and a copy at end-of-data. The daemon
maintains regenerable caches of this information on disk for query speed
and reboot startup, but every cache is reconstructible by mounting the
relevant tape and reading its catalog block.

What the daemon does persist locally:

| State | Storage | Authority |
|--|--|--|
| Audit log | Append-only, fsync'd, hash-chained, file rotated daily | Authoritative — local-only state |
| Per-tape catalog cache | One file per known tape: `/var/lib/rem/cache/<serial>.cat` | Regenerable from tape |
| Library allowlist + per-library policy | `/etc/rem/config.toml` | Authoritative — operator state |
| Library inventory snapshot | In-memory; derived from `READ ELEMENT STATUS` at startup | Tape library is authoritative |
| Drive position + loaded tape | In-memory; queried from drive on demand | Drive is authoritative |
| Active operation registry | In-memory; durable IDs in audit log | Audit log is authoritative for completion |

The catalog cache format on disk is the same format the daemon writes to
tape (§5.8) — so the recovery story is trivial: if the cache is lost, the
daemon mounts the tape and writes a fresh cache file. If both the cache
and the tape are lost, the data is lost; no database recovery shortcut
existed in the previous design either.

The audit log is the only genuinely durable, daemon-owned state. It is
append-only and hash-chained for tamper evidence; periodic checkpoints
are shipped offsite. Format is newline-delimited CBOR (or JSON for
operator readability) — chosen for portability and grep-ability, not
queryability. If query-by-time-range becomes a felt need, SQLite is the
escape hatch — but it would index the audit log, not replace it.

Concurrent access is handled by the daemon being the single writer.
Multiple clients call into the daemon over gRPC; the daemon serialises
cache and audit writes internally. There is no multi-process write
contention to resolve.

---

## 4. Scope and the Remanence / Orchestrator Boundary

### 4.1 What "tape subsystem" means

The fundamental design decision is that **Remanence answers tape questions;
the orchestration layer answers archive questions.**

| Remanence answers | Orchestrator answers |
|--|--|
| What is on tape `L91234L9`? | Where are all copies of `clip_001.mxf`? |
| At what LBA does file `obj-0042` start? | Which files have insufficient redundancy? |
| Read bytes `[10MB, 11MB)` of object `obj-0042`. | Restore this file from the fastest available source. |
| Which tapes are in the library? | What is our total archive size? |
| What is the health of drive bay 3? | Which files should migrate from tape to disk? |
| Write this byte stream to tape, return location. | Ensure this file has the policy-required copies. |

### 4.2 The caller-object-id pattern

When the orchestrator asks Remanence to write data, it supplies a
**caller object identifier** (an opaque string) and optional
**caller metadata** (an opaque structured document). Remanence stores
both as part of its record of that object on tape, and writes them into
the on-tape format alongside the data. Remanence does not interpret the
caller object id; the orchestrator uses it to tie tape-resident objects
back to its own catalog of files.

This is the integration seam. The orchestrator knows what its identifiers
mean and how to map them to logical files. Remanence preserves them
faithfully.

### 4.3 Reconciliation

The orchestrator periodically reconciles its catalog against Remanence: it
queries Remanence for the contents of each tape and confirms that its own
records agree. Discrepancies surface either bugs or out-of-band events
(operator intervention, hardware failure) and are flagged for investigation.
Remanence exposes the queries needed for this reconciliation as part of
its standard API; reconciliation is not a special-case workflow.

---

## 5. On-Tape Format

Remanence draws a hard line between two kinds of on-tape data:

1. **The catalog** — rem's own bookkeeping written at file mark 0 of every
   tape (plus a copy at end-of-data). The catalog format is fixed by
   Remanence, versioned, and **not pluggable**. Anything else would
   produce a bootstrap problem: a tape's catalog can't itself be written
   in an unknown format without an out-of-band channel.
2. **The body** — the actual archived data. The body format is
   **pluggable** via a capability-tier trait system. Different tapes
   may use different body formats; the catalog records which.

### 5.1 Tape layout

Every tape, regardless of body format:

```
[BOT]
  File mark 0:   Catalog block (rem's bookkeeping; fixed format, versioned)
  File mark 1:   Body block 1   (object 1, in the declared body format)
  File mark 2:   Body block 2
  …
  File mark N:   Body block N
  File mark N+1: Periodic catalog refresh (every K objects or T hours)
  …
  File mark M:   Final catalog block (at tape close, written twice)
[EOD]
```

The catalog block records the format identifier of every body block,
its starting LBA, its length, the caller-object-id, and any
per-object metadata the format and the caller supplied.

### 5.2 The catalog format (fixed)

The catalog is the only on-tape structure rem itself defines. It must be
self-describing to a degree that a future reader, with no other context,
can decode it. Format:

- **Magic bytes:** `REM\x00CAT\x01` (8 bytes).
- **Version:** `u32` major.minor.
- **Encoding:** CBOR with a published schema. Field names stable across
  minor versions; major-version bumps require an explicit migration plan.
- **Contents:**
  - Tape identity (barcode, generation, write timestamp, schema version,
    software version that wrote it).
  - Encryption parameters if any (wrapped DEK reference, never the DEK).
  - Per-object entries: object id (rem-assigned UUID), caller-object-id,
    body-format id (e.g. `"rem-chunked-v1"`, `"pax-tar-v1"`), start LBA,
    end LBA, length in bytes, body-format capabilities advertised when
    written, optional checksum, optional per-file LBA index (if the
    format implements `FileAddressable` and chose to inline its index).

The catalog block is read first on every tape mount and cached on disk
(§3.3). A 200,000-object tape's catalog is on the order of 50–100 MB —
trivial to load and query in memory.

### 5.3 Format capability tiers (the body format)

Body formats implement a tiered Rust trait. Each higher tier extends the
capabilities of the lower; a format author opts in to whatever tiers their
format can faithfully support.

#### Tier 0: `TapeFormat` (mandatory)

Every body format implements this. Provides:

- Format identifier and version.
- Capability flags (bitflag declaration of which tiers + orthogonal
  capabilities the format supports).
- Sequential write of objects to a tape block sink.
- Sequential read of objects from a tape block source.
- Capability upcasts (`as_file_addressable()`, `as_byte_range_addressable()`,
  `as_verifiable()`, …) — return `None` by default; format author overrides
  if implemented.

To claim Tier 0 the format must:

1. Be parseable from any start-of-file-mark position without external
   state.
2. Tolerate concatenation: writing object N+1 after object N must produce
   a tape that, on read-back, yields both.
3. Declare its capability flags consistently with which upcast methods
   return `Some`.

#### Tier 1: `FileAddressable` (single-file restore)

A format that lets rem locate and read an individual file inside an
object without scanning from the start of the object.

To claim Tier 1 the format must additionally:

1. **Provide a per-file LBA index.** Either embedded in the format's
   metadata block (read once, kept in memory) or written into rem's
   catalog (§5.2). Retrievable in O(log N) or better.
2. **Use stable file identity.** A `FileId` must uniquely identify a file
   across the object — path-only identity is forbidden because name
   collisions exist in real archives. Acceptable forms: UUID per file,
   or `(path, sequence_no)`, or any opaque bytes the format chooses.
3. **Be independently readable from a file's start LBA.** Seeking to a
   file's start LBA and reading must produce that file's content
   without state inherited from earlier blocks. Chained streaming
   formats (e.g. plain solid-compressed gzip) cannot satisfy this and
   must wrap their content in a chunked container to claim Tier 1.
4. **Be header-recoverable.** If the catalog is corrupt, a forward scan
   from LBA 0 of the object must still enumerate files. File-start
   markers must be self-identifying.

#### Tier 2: `ByteRangeAddressable` (partial file restore / PFR)

A format that lets rem read an arbitrary byte range within a file
without reading or transferring the bytes outside the range.

To claim Tier 2 the format must additionally:

5. **Provide a per-chunk LBA index** within each file. Chunk size is a
   format-author choice; at 1 MB chunks the index cost is roughly 1 KB
   per GB of data, which is acceptable.
6. **Be decodable from any chunk boundary.** Compressed and encrypted
   formats must use restart-capable schemes: zstd seekable frames,
   AES-CTR with per-chunk IVs, etc. Solid-compressed xz or chained-IV
   AES-CBC streams cannot claim Tier 2 without an outer chunked
   container.
7. **Expose `seek_granularity()`** — the smallest byte range the format
   can locate without reading + discarding intra-chunk bytes. Caller
   uses this to plan reads and report "fastest possible response" to
   the orchestrator.

#### Orthogonal capabilities

In addition to the linear tier 0 → 1 → 2 hierarchy, formats may
implement orthogonal capabilities, each declared by a flag and
optionally backed by an extension trait:

| Capability | Meaning |
|--|--|
| `VERIFIABLE` | Per-object checksums recorded in the format and verified on read. |
| `APPENDABLE` | New objects can be added to an existing tape. Disabled by default for WORM workflows. |
| `RESUMABLE_WRITE` | A write interrupted partway can resume from a checkpoint without rewinding to BOT. |
| `SPARSE_PRESERVING` | File sparseness is preserved end-to-end. |
| `METADATA_PRESERVING` | POSIX ACLs, xattrs, nanosecond-resolution mtime, etc. preserved. |
| `ENCRYPTED` | The body bytes are AES-256-GCM encrypted by the LTO drive; format records key wrapping parameters but never key material. |
| `COMPRESSED` | The body bytes are compressed at the format level (not just hardware-level). |

A format declares its full set of capabilities via `capabilities() -> Capabilities`
and the daemon either runs against them or — for ops the format does not
support — surfaces `Unsupported` to the caller before any tape motion.

### 5.4 Format registry

Format implementations are registered with the daemon at startup. Two
mechanisms:

1. **In-tree formats** are registered explicitly in `main` by the
   daemon binary. The built-in set is small (see §5.7).
2. **External formats** are loaded as Rust dependencies and registered
   the same way. Out-of-tree formats become a recompile-and-redeploy of
   the daemon — there is no dynamic loader. This is intentional:
   loading binary plugins into a long-lived daemon with hardware access
   widens the attack surface in ways that are not worth the convenience.

When the daemon opens a tape, it reads the catalog block, finds each
object's body-format id, and looks up the corresponding factory in the
registry. If a format id is unknown, the daemon refuses to operate on
that object (reads return `UnknownFormat`; the tape remains intact).
Older formats are kept registered as long as any tape in the fleet
uses them — formats are a forever decision.

### 5.5 LBA-based seek (PFR mechanics)

The LTO command set supports two seek primitives that make file- and
byte-level restore tractable:

- **LOCATE BLOCK** — seek directly to a given LBA. Realistic latency:
  10–100 seconds typical, ~60 seconds average for BOT→middle on LTO-9,
  ~100 seconds end-to-end worst case. Cold LOCATE on a loaded tape is
  the dominant cost of any single PFR call.
- **SPACE BLOCKS** — skip forward (or backward) N blocks from the
  current position. Faster than LOCATE for short forward distances
  where the cost of computing the target LBA outweighs the seek.

The flow for a byte-range read against a `ByteRangeAddressable` format,
given a caller request `read(object_id, file_id, [U_start, U_end))` and
a uniform `chunk_size` declared in the format header:

1. Catalog says object `obj-0042` is at LBA range `[X, Y)`. Per-file
   index says file `foo.dat` starts at chunk `first_chunk` within the
   object and has length `F.file_size` bytes.
2. Compute the chunks covering the request:

   ```
   first_chunk_within_file = U_start / chunk_size                  // floor
   last_chunk_within_file  = (U_end - 1) / chunk_size
   chunks_to_read          = last_chunk_within_file - first_chunk_within_file + 1
   first_abs_chunk         = F.first_chunk + first_chunk_within_file
   start_lba               = chunk_table[first_abs_chunk].lba
   ```

3. Daemon issues **one** LOCATE BLOCK to `start_lba`.
4. Daemon reads `chunks_to_read` blocks. The rem-chunked-v1 invariant
   "one chunk per tape block" makes each block = one zstd-seekable
   frame, decodable independently.
5. Daemon trims. The total decompressed byte count must account for a
   possibly-short last chunk: if the read includes the file's last
   chunk, that chunk contributes `F.file_size mod chunk_size` bytes (or
   `chunk_size` if the file size is an exact multiple), not the nominal
   `chunk_size`.

   ```
   F.last_chunk_within_file = (F.file_size - 1) / chunk_size
   short_last_size          = F.file_size - F.last_chunk_within_file * chunk_size
   total_decompressed       = chunks_to_read * chunk_size
   if last_chunk_within_file == F.last_chunk_within_file:
       total_decompressed   = total_decompressed - chunk_size + short_last_size
   head_drop                = U_start - first_chunk_within_file * chunk_size
   tail_drop                = total_decompressed - (head_drop + (U_end - U_start))
   ```

   Drop the first `head_drop` bytes and the last `tail_drop` bytes of
   the concatenated decompressed output, then stream the rest.
   `pfr-reference.md` §4.4 walks through the short-last-chunk case
   with a worked example.

For a request `[800 MiB, 802 MiB)` with `chunk_size = 1 MiB`:
two chunks (LBA `start_lba` and `start_lba + 1`), one LOCATE, two
block reads, no trim. Worst-case sub-chunk request reads one block
and discards up to `chunk_size − 1` bytes of head + tail.

End-to-end latency for a cold byte-range read on a loaded tape:
LOCATE (~30–60 s typical) + N chunk reads (sub-second each at LTO-9
line rate). Hot reads (tape already positioned past the target,
chunk count small) are sub-second total. See `docs/pfr-reference.md`
for the full hardware substrate, real-world latency numbers, the
encryption interaction (per-block GCM requires chunk == block), and
LTO-9's oRAO batch accelerator.

### 5.6 What rem itself does *not* manage

- Cross-tape file spans. A file must fit on a single tape. Multi-tape
  archives are the orchestrator's responsibility. (Reasoning: cross-tape
  PFR ranges are operationally awful; multi-volume tar extensions are
  historically fragile.)
- Per-object encryption keys. The drive does the encryption; rem stores
  wrapped key references in the catalog.
- Format conversion. There is no `rem convert-format`. To migrate a tape
  from one body format to another, the orchestrator reads from the old
  format and writes to the new on a different tape.

### 5.7 Built-in formats

Two body-format implementations ship in tree:

| Format id | Tier | Notes |
|--|--|--|
| `pax-tar-v1` | Tier 0 + Tier 1 (with sidecar index in rem's catalog) | POSIX pax tar — the maximum-portability option. Forty-five years of stable tooling. A tar tape written today is readable in 2070 with any Unix-like system's `tar` command. Tier 2 (PFR) is not supported because POSIX tar has no native chunk boundaries. |
| `rem-chunked-v1` | Tier 0 + Tier 1 + Tier 2 + `VERIFIABLE` + `METADATA_PRESERVING` | The rem-native default. Chunked container with 1 MB chunks (configurable per write session), zstd-seekable compression per chunk (optional), per-chunk SHA-256, per-file index inlined in object header, per-chunk index inlined per file. Documented format spec; CBOR-serialised metadata. |

`rem-chunked-v1` is the default unless the orchestrator requests
otherwise.

Third-party formats anticipated (not in-tree, not committed to):
`rem-format-bacula`, `rem-format-bareos` — useful for migration from
existing tape systems. Tier 0 only is likely; PFR support depends on
whether the source format's index structure is recoverable.

### 5.8 The catalog-on-tape property

Taken together, the catalog block at file mark 0, the per-object format
metadata, the periodic catalog refresh, and the final catalog block at
end-of-data form a complete self-describing record of the tape. The
on-disk cache is a fast index for queries, but the tape itself is the
durable record. If every cache file is lost, every tape can be re-walked
and its full contents reconstructed.

### 5.9 MAM (Medium Auxiliary Memory)

Every LTO cartridge contains a small writable memory chip accessible via
SCSI READ ATTRIBUTE / WRITE ATTRIBUTE. Remanence uses this for:

- Latest catalog block position (LBA) for fast access.
- Total objects written and total bytes written.
- Last write timestamp.
- Tape state flags (writing in progress, finalized, suspect, retired).
- Schema version of the on-tape catalog.

MAM is a hint, not a source of truth. The authoritative state is on the
tape itself (in the catalog blocks). MAM speeds up common operations
and provides a useful breadcrumb after unexpected events.

---

## 6. Hardware Abstraction

(§6.1 through §6.5 are carried forward from v0.2 with minor edits;
the substantive content is unchanged. The OS-portability seams listed
in §2.2 cross-cut this section: device enumeration in §6.1, hot-plug
in §6.3, identifiers in §6.4 — all are designed to admit non-Linux
implementations behind a trait.)

### 6.1 The drive enumeration problem

A foundational operational problem with SCSI tape libraries is that the
kernel-assigned device nodes (`/dev/sg*`, `/dev/nst*` on Linux;
`\\.\TapeN`, `\\.\ChangerN` on Windows) do not stably correspond to
physical drive bays in the library. After a reboot, an HBA rescan, or a
drive replacement, the same physical drive may appear under a different
device node. Software that hard-codes device paths breaks when this
happens. The daemon's device-discovery layer is an OS-portability seam
(§2.2): a `DeviceEnumerator` trait with a Linux implementation today.

### 6.2 The join-by-serial solution (empirically grounded)

Remanence never persists kernel device paths. The runtime mapping is
derived fresh on each rediscovery — at process start, on explicit
`LibraryService.Refresh` calls, and (with Layer 2c's watcher in
tree as of 2026-05-18) **on hot-plug events** once the daemon
(Layer 5) consumes the watcher's burst stream. The mapping is built
by issuing two cheap queries and joining their results by serial
number:

1. **From the library**, READ ELEMENT STATUS with both the `DVCID` and
   `CurData` bits set (CDB byte 6 = `0x03`), `element_type=4` (Data
   Transfer Elements). The response includes a 34-byte identifier
   descriptor per drive containing the drive's vendor, product, and
   serial number.
2. **From the kernel**, enumerate tape devices, issue INQUIRY + VPD page
   0x80 to each, and read the serial.

Joining these two mappings by serial number yields the runtime
correspondence between library bay and host device.

> **Why both bits, not just DVCID?** v0.1 of this spec assumed (per HPE's
> published SCSI reference) that DVCID alone would suffice. Empirical
> testing during the Layer 1 build-out — confirmed against the production
> MSL3040 firmware 3350 and against QuadStor VTL — found that HPE firmware
> silently omits the identifier block unless CurData=1 is also set in the
> CDB.

#### 6.2.1 Fallback when DVCID is unavailable

For libraries that don't honor the DVCID+CurData combination (vendor
firmware we have not yet tested — including the Overland XL80 — may
behave differently), Remanence's discovery falls back through:

1. Retry with `element_type=4` (drives-only) if the all-types form was used.
2. Retry with CurData inverted.
3. Final fallback: derive the bay-to-serial mapping from the SCSI bus
   topology — drives on the same `host:channel` as the changer, sorted by
   SCSI ID — and emit a discovery warning that the result depends on a
   vendor-specific convention.

Discovery is not failed by these fallbacks; the operator is informed via
warnings in the discovery report.

### 6.3 Hot-plug integration (Layer 2c)

Layer 2c subscribes to OS hot-plug events on the SCSI subsystems
and emits coalesced notification bursts that the consumer (daemon)
uses to trigger re-discovery on add, remove, and change events.
The watcher's model is "notify; consumer decides what to do" — it
never calls SCSI itself. The consumer's intended model is "re-derive
on event," not "incrementally update on event" — the cost of full
re-enumeration is sub-second even for fully-populated multi-chassis
libraries, and the simpler model avoids edge cases where multiple
things change at once. On Linux the event source is udev; on Windows
it will be `RegisterDeviceNotification` (planned, not implemented).

The Linux backend ships in `remanence-library::watch` behind the
`linux-udev` Cargo feature (requires `pkg-config` + `libudev-dev`
system packages at build time). Cross-platform scaffolding (event
types, trait, mock source, coalescer state machine) builds
everywhere. See `docs/layer2c-design.md` for the design rationale
and `INSTALL.md` for the build runbook.

**Daemon integration is still TBD.** The watcher emits bursts; the
daemon's policy for *what to do* with each burst (call `refresh()`,
`rescan()`, or full `discover()` on which subset of owned libraries)
lives in Layer 5, which is unimplemented. Until then, callers can
exercise the watcher directly via `rem watch` (also feature-gated)
or by subscribing programmatically through `LinuxUdevSource`, and
manually invoke `LibraryHandle::refresh()` on bursts.

**Without Layer 5:** discovery runs at process startup and on
explicit `LibraryService.Refresh` invocations. Cartridge moves done
by an operator outside rem (front-panel button, web UI, another
tool) are invisible until the next refresh — the operator must
trigger one, or the orchestrator must call refresh on a cadence
that suits its workflow. Layer 2c removes the manual step.

### 6.4 Identifiers

| Entity | Stable identity | Source |
|--|--|--|
| Library | Library serial (e.g. `DEC418146K_LL02`) | Changer VPD 0x80 |
| Drive | Drive serial number | Drive VPD 0x80 / inline DVCID identifier in RES |
| Drive bay | `(library_serial, element_address)` | Composite — stable across drive swaps |
| Slot | `(library_serial, element_address)` | RES element address |
| Cartridge | Barcode (volume tag) | RES descriptor / library's barcode reader |
| Object on tape | `(tape_barcode, object_uuid)` | Assigned by rem at write time; recorded in catalog |
| File within object | `(object_uuid, FileId)` | Format-defined opaque identifier |
| OS device node | **not stable** | Treat as labels-of-the-day |

No identifier in Remanence's persistent state is platform-specific or
installation-specific. A configuration can be moved between hosts
without remapping.

### 6.5 One model for partitioned and physically-separate libraries

(Carried forward from v0.2 §6.5 unchanged.)

The deployments Remanence will see — a single unpartitioned library; a
single chassis partitioned into N logical libraries; M physically separate
unpartitioned libraries; any mix — all reduce to **a flat list of logical
libraries**. Operations are gated on explicit per-library opt-in: the
daemon refuses to issue state-changing commands against any library not
on its allowlist.

---

## 7. External API

### 7.1 Transport

Remanence exposes its API as a **gRPC service over TLS 1.3 with mutual TLS
authentication**. Default port 8443. Plain TCP is not supported in
production deployments; an unauthenticated local-development mode (Unix
socket only, no network listener) is available for testing.

Why gRPC (over REST + SSE):

- **Native streaming.** Tape data paths are inherently streamed: writes
  of multi-GB objects, reads of multi-GB byte ranges, multi-hour
  verification runs with progress. gRPC handles bidirectional streams
  as a first-class concept; HTTP+JSON does not.
- **First-class cancellation *requests*.** A client closing a stream
  signals a cancellation request to the server. For SCSI tape and
  changer operations, cancellation is **best-effort and asynchronous**
  — see §7.3 for the safe-point model and the terminal-state vocabulary
  the daemon uses to honestly report what actually happened.
- **Strongly typed schemas.** API drift is caught at code review (the
  `.proto` file is the source of truth); breaking changes are explicit;
  generated clients in any language come for free.
- **Bidirectional event delivery.** Replaces the v0.2 SSE stream with a
  bidi gRPC stream on which the server pushes events and the client
  can acknowledge or filter.

mTLS gives cryptographic authentication with no shared secret to leak,
supports per-client revocation, and integrates with operational best
practices (short-lived certs, automated rotation via step-ca or
equivalent). Same posture as v0.2.

A REST/JSON gateway can be added later via `grpc-gateway` (or
equivalent) if a future orchestrator requires HTTP/JSON access. It is
not a v0.3 deliverable.

### 7.2 Service model

A small, opinionated set of gRPC services:

- **`LibraryService`** — list libraries, inspect a library's drives/slots/
  IE ports, refresh inventory, rescan after operator intervention.
- **`TapeService`** — list known tapes, inspect a tape's catalog (object
  list, LBAs, body format), look up an object by caller-id.
- **`WriteService`** — explicit-session write lifecycle.
  - `OpenSession(tape_id) → session_id` picks a free drive, MOVEs the
    cartridge, LOADs the drive, returns when READY. Streams progress
    events for each phase.
  - `AppendObject(session_id, …) → stream<Progress>` writes one object;
    callable many times within a session. Bidi stream: client streams
    object bytes + manifest fragments; server streams back progress.
  - `Commit(session_id)` writes the final catalog block and returns.
  - `CloseSession(session_id)` UNLOADs and returns the cartridge to
    its home slot.
- **`ReadService`** — explicit-session read lifecycle, symmetric with
  `WriteService`. The orchestrator owns the queue and ordering;
  the daemon owns the drive mechanism.
  - `OpenSession(tape_id) → session_id` picks a free drive, MOVEs the
    cartridge, LOADs the drive, returns when READY. Streams progress
    events through `Moving → Loading → Calibrating → Ready`. Fast-
    path: if the requested tape is already loaded in a drive (e.g.
    left there by a prior session), rem skips MOVE+LOAD and returns
    READY immediately — invisible optimisation, never a slow path
    the caller didn't ask for.
  - `ReadRange(session_id, object_id, file_id, byte_range) → stream<bytes>`
    performs one byte-range read against the currently-loaded tape.
    Implementation per `docs/pfr-reference.md` §4: catalog lookup →
    one LOCATE → chunk-aligned reads → head/tail trim. Tape stays
    loaded between calls. Callable many times in any order; the
    daemon does not reorder.
  - `CloseSession(session_id)` UNLOADs the drive and MOVEs the
    cartridge back to its home slot. rem never keeps a tape loaded
    after `CloseSession` "in case the caller comes back" —
    introducing eviction heuristics would force rem to invent policy
    it doesn't own.
  - Failure surface: `OpenSession` returns `TapeInUse(session_id)` if
    the requested cartridge is held by another active session,
    `NoDriveAvailable` if all compatible drives are busy. A drive
    fault mid-session transitions the session to `Failed`; in-flight
    `ReadRange` streams receive an error. Idle-timeout (configurable;
    default ~30 min of inactivity) auto-closes a forgotten session
    and records `ClosedByTimeout` in the audit log. Idle-close is a
    safety net, not a feature; orchestrators should call
    `CloseSession` explicitly.
- **`OperationService`** — track and request cancellation of long-
  running operations. Server-streamed for progress. Cancellation
  semantics per §7.3: requesting a cancel does not guarantee one.
- **`EventService`** — bidi event stream for hot-plug, alerts, audit
  notifications.
- **`AdminService`** — read-only mode toggle, emergency stop, cert
  revocation acknowledgement.

Idempotency: write-initiating RPCs accept an `idempotency_key` (UUID)
in the request metadata. If a client retries after a network failure,
the server returns the original operation rather than creating a
duplicate. This is critical for tape operations.

### 7.3 Long-running operations and cancellation

Tape operations span seconds to hours (an LTO-9 first-load calibration
alone can take up to two hours). All long-running operations are
modelled as gRPC streams. **Cancellation is best-effort, not a
guaranteed rollback.** Once a SCSI MOVE, LOAD/UNLOAD, LOCATE, WRITE
FILEMARKS, or long write is in flight, the daemon often cannot stop
the hardware mid-command — and even when it can, the physical state
(cartridge in transit, tape positioned partway through a write) may
not match what either side expected. The model:

- The RPC returns immediately with an operation id; the response stream
  yields progress events until the operation reaches a terminal state.
- Closing the client stream sends a **`CancelRequested`** signal. The
  daemon decides what to do based on the operation's current safe point.
- Each operation declares its safe points — points at which cancellation
  can be honoured cleanly. Examples:
  - A multi-object write session has a safe point between objects
    (commit boundary).
  - A multi-chunk PFR read has a safe point between chunks.
  - A single SCSI MOVE has only two safe points: before dispatch, and
    after the drive returns final status.
- Terminal states the daemon may emit, all explicit in the `.proto`:
  - `Succeeded` — completed as requested.
  - `Failed` — completed with a SCSI or transport error; no
    cancellation involvement.
  - `CancelledBeforeDispatch` — `CancelRequested` arrived before the
    daemon issued the first state-changing CDB; nothing happened.
  - `CompletedAfterCancel` — the daemon received `CancelRequested`,
    could not interrupt the in-flight CDB, and waited for it to
    complete successfully. The hardware did the operation; the caller
    asked it not to.
  - `CancellationRejected` — the operation is past its last safe
    point (e.g. mid-MOVE) and `CancelRequested` was acknowledged but
    not acted on; the daemon will report `Succeeded` or `Failed`
    based on what actually happens.
  - `CompletionUnknown` — the daemon lost contact with the drive
    mid-CDB (transport timeout, kernel I/O error) after attempting
    to cancel; physical state must be re-derived by rescan. Carries
    a `DirtyCause::CompletionUnknown` marker if a snapshot exists.
- Resume after disconnect: the client reconnects with the operation id;
  the server replays missed progress events from a per-operation ring
  buffer. Reconnecting does **not** auto-cancel; the operation continues
  unless the reconnect explicitly carries a `CancelRequested`.

The Layer 2b dirty-state machine (already implemented for state-
changing changer operations) is the model: completion-unknown
transport errors leave the in-memory snapshot dirty with a precise
cause, and the operator-facing API surfaces the cause rather than
papering over it. Layer 3 and the gRPC layer extend the same
discipline upward.

### 7.4 Authorization

Authorization derives from the client certificate. Roles encoded in the
certificate subject determine the permission set:

- `orchestrator` — full access to write, read, and library management.
- `operator` — admin access including drive cleaning, import/export, but
  not write operations.
- `readonly` — query access to library state, tape contents, operation
  history.
- `admin` — additionally authorised for sensitive operations (read-only
  mode toggle, emergency stop).

Role-to-permission mapping is explicit and enumerable. Authorization is
scoped not just by role but by library: a certificate's subject may
include a `libraries` attribute restricting the client to a specific
set of library serials.

### 7.5 Reference client

The `rem` CLI is the reference client and a thin wrapper over the gRPC
service. A Python client library (`remanence-client`) is generated from
the `.proto` schema with hand-tuned ergonomics; both the CLI and the
Python client serve as integration references.

---

## 8. Security

(§8.1 through §8.4 carried forward from v0.2 unchanged in substance.
Two minor edits:

1. The "append-only audit log" bullet in §8.2 now references the local-
   state layer (§3.3) rather than a database.
2. The "catalog backup" bullet in §8.2 is reworded: instead of "daily
   encrypted backups of the catalog database," we describe daily
   audit-log checkpoints + the per-tape self-description. Tape-content
   recovery does not depend on a daemon-local catalog.)

### 8.1 Threat model

Remanence assumes a hostile network. Defenses are layered accordingly.

**In scope:** network-based attackers, compromised credentials of
legitimate clients, insider threats, ransomware reaching the daemon
host, tampering with the audit log.

**Out of scope:** physical access to the library, root-level compromise
of the daemon host (assumed game-over), supply chain attacks against
the build toolchain, side-channel attacks against the host hardware.

### 8.2 Defense in depth

- **Cryptographic authentication.** mTLS for every API client.
- **Least privilege.** Daemon runs as unprivileged user with only the
  OS capabilities and device permissions it strictly needs.
- **Process hardening.** systemd unit applies `ProtectSystem=strict`,
  `ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`,
  `RestrictAddressFamilies`, and related options.
- **Library allowlist.** Explicit list of library serials the daemon is
  allowed to issue state-changing commands against.
- **Append-only audit log.** Hash-chained, periodically checkpointed
  offsite. Tampering is detectable. The audit log is local state
  (§3.3), not a database.
- **Anomaly thresholds.** Destructive operations beyond normal rates
  require explicit override.
- **No destructive primitives.** No API endpoint destroys data. Tapes
  can be retired, exported, and scratched only by deliberate operator
  action with explicit confirmation.
- **Read-only and emergency stop modes.** A single privileged operation
  puts the daemon into a state where it serves reads and refuses
  writes/moves.
- **Tape-content recovery without daemon state.** Because every tape
  self-describes (§5.8), losing the daemon's local cache does not lose
  tape contents — a fresh daemon mounts the tape and rebuilds the
  cache. Audit-log checkpoints are shipped offsite for tamper
  detection, not for tape-content recovery.

### 8.3 Certificate management

Certificates are issued by a private CA managed via `step-ca` or
equivalent. Short certificate lifetimes (90 days), automated renewal,
revocation via CRL or OCSP. The CA itself is offline or air-gapped from
the daemon host.

### 8.4 Encryption of tape contents

Remanence supports per-tape encryption using LTO hardware encryption
(AES-256-GCM) via the SCSI SECURITY PROTOCOL IN/OUT commands.
Encryption is configured per write session.

#### 8.4.1 Key architecture

Two-tier key model:

- A **Key Encryption Key (KEK)** is held outside Remanence in a hardware-
  backed key store (typically YubiKey PIV applet, with offline backup
  copies under operator control). The KEK never leaves the hardware.
- **Per-tape Data Encryption Keys (DEKs)** are generated at write time.
  The DEK is sent to the drive via SECURITY PROTOCOL OUT for the
  duration of the session, wrapped by the KEK for catalog storage, and
  forgotten by the drive at unload.

The wrapped DEK is stored in the on-tape catalog (§5.2) and in the
disk catalog cache. To read an encrypted tape, the wrapped DEK is
retrieved, unwrapped via KEK access (an explicit, audited operation),
and sent to the drive.

#### 8.4.2 Key recovery

Threshold cryptography (Shamir's secret sharing) is used to split the
KEK across multiple custodians, with a threshold smaller than the
total. Protects against single-point loss and single-point compromise.

#### 8.4.3 What is encrypted

The orchestration layer decides on a per-tape basis whether to encrypt.
Typical deployment writes three copies of archive data with one copy
encrypted (defense against unencrypted-copy physical compromise).

---

## 9. Open Questions and Deferred Decisions

### 9.0a Session lifecycle edge cases (implementation TBD)

§7.2 defines the happy-path `OpenSession` / `ReadRange` (or `AppendObject`)
/ `CloseSession` flow. Several edge cases need precise rules before the
gRPC layer ships. Listing them here so the implementation phase
addresses them deliberately:

- **Client disconnect mid-session.** The gRPC stream dies but the
  daemon-side session is still open. Lean: enter `OrphanedSession`
  state and start the idle-timeout countdown immediately rather than
  from-last-activity. Auto-close on timeout, audit-log it.
- **Client reconnect to a session.** Should the session id be
  reusable? Lean: yes — a client can `OperationService.Resume(session_id)`
  to reattach a progress stream as long as the session is not yet
  `Closed`. Useful when the client process restarts.
- **Idle timeout policy.** Default ~30 min of no activity. Should be
  configurable per-deployment and per-session at open time (the
  orchestrator may know it has work to do for the next hour).
- **Daemon restart.** All in-memory sessions are lost. On startup, the
  daemon issues an unload of every drive (or a status check + unload-
  if-loaded) to put the library in a known state, then re-derives
  inventory. Audit log records `LostByRestart` for each session that
  was active. The orchestrator must treat any session id it held
  across a daemon restart as invalid.
- **Tape found loaded outside an active session.** A drive may come
  up with a cartridge already loaded (operator left it there, prior
  daemon crash, manual load). `OpenSession` for that tape should
  fast-path (skip MOVE+LOAD); `OpenSession` for a *different* tape
  on that drive should treat the loaded tape as "occupying the
  drive" but not "in a session" — and refuse to use that drive
  until the loaded tape is moved back via a `LibraryService.RecoverDrive`
  admin call. Never auto-eject a found tape; the operator may have
  loaded it deliberately for a non-rem workflow.
- **SCSI reservation contention.** Another initiator (different host
  on the same SAS fabric, another tool's process) may hold a SCSI
  reservation on the drive. Detect via reservation conflict status
  on first command; return `DriveReservedByOtherInitiator`. Out of
  scope for the typical deployment but should not panic.

These are not blockers for v0.3 (which is design-only). They're a
checklist for the Layer 5 + Layer 3a implementation phase.

### 9.0 oRAO and batched reads — deferred

LTO-9 full-height drives expose oRAO (open Recommended Access Order)
via `Receive Recommended Access Order` (SSC). The application submits
a list of read targets; the drive returns them in serpentine-traversal
order; first-byte access time drops by up to 73% in published
benchmarks. **Not implemented in v0.3.** Reasoning:

- The target fleet is LTO-9 half-height (no oRAO exposure), plus LTO-7 and
  LTO-6 drives (predate oRAO entirely). Zero hardware benefit today.

**Forward path if hardware ever changes.** The v0.3 contract is
strict: Layer 3a does not queue or reorder reads, and `ReadRange`
calls execute in the order the caller issues them (§3.2, §7.2).
Adding oRAO transparently underneath `ReadRange` — by batching
overlapping in-flight reads and reordering them inside the daemon —
would violate that contract and make latency unpredictable. Earlier
drafts of this spec suggested it; codex flagged the contradiction
and we are taking the strict path.

If oRAO becomes relevant later (LTO-9 full-height bays added, LTO-10
brings oRAO to half-height), the right move is a *new explicit*
batch API — a `ReadService.BatchReadRange` RPC that takes a
list of `(object_id, file_id, byte_range)` tuples scoped to one
session, issues them to the drive via `Receive Recommended Access
Order`, and streams results back in drive-recommended order. The
orchestrator opts in by choosing the batch RPC over individual
`ReadRange` calls; non-opt-in callers retain the original contract.
That work is small (one SCSI command in Layer 1, one RPC in Layer 5)
and orthogonal to v0.3.

### 9.1 The `rem-chunked-v1` format spec

§5.7 names this as the default body format and §5.3 lists the trait
contract it must satisfy. The detailed CBOR schema, chunk-size defaults,
zstd seekable-frame parameters, per-chunk checksum policy, and the
metadata-preservation field set are still open. **Status: required for
M3/M4.** Best decided when the format crate begins implementation.

### 9.2 The catalog block schema

§5.2 lists fields; the precise CBOR schema, field tag assignments, and
the major-version migration policy are open. **Status: required for M4.**
This is a forever decision; deserves deliberate review.

### 9.3 SCSI reservation strategy

How Remanence uses PERSISTENT RESERVE IN/OUT for drives and the changer.
Leaning "minimal" — logical-library isolation in the firmware handles
separation between Remanence and any other tool sharing the same chassis,
so changer-level reservations are not necessary in the typical deployment.
Per-drive PERSISTENT RESERVE during active write or read sessions provides
robustness against concurrent access by a misbehaving second initiator.
Exact reservation parameters settle with the first Layer 3 write-session
implementation.

### 9.4 Write verification policy

Should every object written be immediately verified by reading back and
checksumming? Always, sometimes, never? Decision likely depends on
observed error rates with the specific MSL3040 + LTO-9 combination.
**Status: still open.**

### 9.5 Multi-library and mixed-vendor deployments

A single daemon manages every reachable logical library. What remains
open is operational, not architectural:

- Cross-library moves are impossible at the SCSI level. The API surfaces
  this as a discoverable property rather than failing on attempts.
- Tape-pool semantics across libraries — default leaning: pools are
  library-scoped; the orchestrator coordinates cross-library moves.
- Identifier disambiguation in operator UX.

These are operator-facing decisions for the operator documentation.

### 9.6 Cross-tape file spans

§5.6 forbids them in v0.3. Worth revisiting if the orchestrator presents
a strong use case, but PFR across tape boundaries is operationally
awful and we'd want to see real demand first. **Status: forbidden.**

### 9.7 Operator UI

Remanence exposes an API but not a user interface. Whether to ship a
reference TUI (`ratatui`-based) and/or web UI as part of the project,
or to leave that to the orchestration layer, is an open question.
**Status: still open.**

### 9.8 OS portability beyond Linux

§2.2 mandates that architectural seams admit non-Linux implementations,
but no actual Windows port is committed for v1. Whether to actively
land Windows transports in tree, or to wait for an outside contribution,
is open. **Status: portable by construction; not actively ported.**

### 9.9 Distribution and licensing

**Resolved (v0.2):** AGPL-3.0-or-later. Source repository is private
during initial development; will be made public once the core layers
are functional. Packaging strategy (deb/rpm packages, container images,
source-only) is still open.

### 9.10 Vendor coverage beyond HPE

Layer 1 fixtures exhaustively cover the HPE MSL3040 / Ultrium 9 / Ultrium 7.
Coverage for the Overland Storage XL80 and its LTO-6 / LTO-7 drives is
**pending capture.** Same capture scripts apply.

---

## 10. Implementation Roadmap

| Milestone | Description | Status (May 2026) |
|--|--|--|
| M1 — SCSI core (Layer 1) | INQUIRY, VPD pages, READ ELEMENT STATUS (with DVCID + CurData), MOVE MEDIUM, INITIALIZE ELEMENT STATUS, PREVENT/ALLOW, LOG SENSE. Tape-side commands: LOAD/UNLOAD, LOCATE, READ POSITION, SPACE, WRITE FILEMARKS, raw READ/WRITE. | **Partial — SMC-3 side complete and live-tested. Tape-side commands pending.** |
| M2a — Library discovery + identity | Cold enumeration, join-by-serial, library/drive/slot value types. | **Complete — live on QuadStor + production MSL3040.** |
| M2b — Library state-changing ops | Move, load, unload, import, export, dirty-state machine. | **Complete — live on QuadStor + production MSL3040.** |
| M2c — Hot-plug watcher | Subscribe to OS hot-plug events, emit coalesced bursts so the consumer can trigger re-discovery. Linux udev today (feature-gated); Windows `RegisterDeviceNotification` future. | **Implementation complete; live-tested on akash 2026-05-18. Daemon-side wiring of bursts → `refresh()` is Layer 5's job, not started.** |
| M3a — Tape mechanism (Layer 3a) | Block-level LOAD/LOCATE/READ/WRITE wrapper with streaming + progress + cancellation. No format awareness. | **Not started.** |
| M3b — Tape format (Layer 3b) | `TapeFormat` trait + tier traits, format registry, `rem-chunked-v1` and `pax-tar-v1` implementations, catalog block reader/writer. | **Not started.** |
| M4 — Local state (Layer 4) | Append-only audit log, per-tape catalog cache, config. **No database.** | **Not started.** |
| M5 — API (Layer 5) | gRPC server with mTLS, `.proto` schemas, idempotency, audit emission, generated Python client. | **Not started.** |
| M6 — Encryption | SECURITY PROTOCOL IN/OUT. YubiKey integration. KEK / DEK / Shamir recovery. | **Not started.** |
| M7 — Operational hardening | systemd unit, anomaly thresholds, rate limiting, read-only mode, emergency stop, audit-log offsite checkpoint automation. | **Not started.** |
| M8 — OS portability seams | Document `DeviceEnumerator` / `PrivilegeAdvice` traits, verify no Linux types leak into Layers 2+, accept Windows contributions when offered. | **Ongoing — discipline, not a deliverable.** |
| M9 — Operator UI (optional) | TUI and/or web UI. | **Not started.** |
| M10 — Production readiness | Documented procedures, recovery drills, deployment automation, Prometheus metrics, tested upgrade path. | **Not started.** |
| M11 — Third-party formats (community) | `rem-format-bacula`, `rem-format-bareos` if contributed. | **Not started.** |

Milestones 1–5 are sufficient for a useful tool. Milestones 6–8 are
required for any production deployment. Milestones 9–10 turn the
production-ready tool into a maintainable, supportable system.

---

## Appendix A: Glossary

| Term | Definition |
|--|--|
| **Barcode** | The human- and machine-readable label affixed to a tape cartridge; primary identifier for the tape. |
| **BOT / EOT** | Beginning of Tape / End of Tape — physical reference positions on the tape medium. |
| **CDB** | Command Descriptor Block — the binary structure that defines a SCSI command. |
| **CurData** | "Current Data" bit in the READ ELEMENT STATUS CDB (byte 6 bit 0). Empirically required alongside DVCID on HPE firmware. |
| **DEK** | Data Encryption Key — a per-tape symmetric key used for hardware encryption of tape contents. |
| **DVCID** | Device Identifier bit in the READ ELEMENT STATUS CDB (byte 6 bit 1). |
| **EOD** | End of Data — the position on tape after the last written data. |
| **`FileAddressable`** | Tier 1 capability — locate and read individual files within an object without scanning. |
| **`ByteRangeAddressable`** | Tier 2 capability — read arbitrary byte ranges within a file (Partial File Restore). |
| **File mark** | A tape-internal marker separating distinct files or records. |
| **gRPC** | Google Remote Procedure Call — RPC framework with native streaming, used as the primary API transport. |
| **KEK** | Key Encryption Key — a master key used to wrap and unwrap per-tape DEKs. |
| **LBA** | Logical Block Address — the seek primitive on LTO tape. |
| **LOCATE BLOCK** | SCSI command (`0x92`) to seek the tape to a given LBA. |
| **LTFS** | Linear Tape File System (ISO/IEC 20919). **Explicitly not used** by Remanence. |
| **LTO** | Linear Tape-Open. |
| **MAM** | Medium Auxiliary Memory — writable memory chip inside each cartridge. |
| **mTLS** | Mutual TLS. |
| **Partitioning** | A vendor feature splitting one chassis into several logical libraries. |
| **Pax format** | A POSIX-standardised extension of tar. |
| **PFR** | Partial File Restore — reading an arbitrary byte range within a file. |
| **Picker** | The robotic mechanism in a tape library. |
| **SCSI** | Small Computer System Interface. |
| **SMC-3** | SCSI Media Changer-3. |
| **SPC-5 / SSC-5** | SCSI Primary Commands / SCSI Stream Commands. |
| **SPTI** | SCSI Pass-Through Interface (Windows equivalent of SG_IO). |
| **`TapeFormat`** | Tier 0 capability — every body format implements this. |
| **udev** | The Linux device-event subsystem. |
| **VPD** | Vital Product Data — a SCSI mechanism for retrieving structured device information. |
| **WWN (NAA)** | World-Wide Name in IEEE NAA format. |

---

## Appendix B: Changes from v0.2

| § | Change |
|--|--|
| Front-matter | Six foundational decisions added; supersedes v0.2. |
| 3.2 | Layer 2 split into 2a (discovery, complete) / 2b (state changes, complete) / 2c (hot-plug watcher). 2c was originally listed as pending; it landed on 2026-05-18 with live-smoke verification on akash. Daemon-side wiring of bursts → `refresh()` remains Layer 5's job. v0.3's first cut overstated Layer 2 completion; this row tracks the corrected state. |
| 5.5 | PFR worked example corrected — a `[800 MiB, 802 MiB)` range with 1 MiB chunks reads **two** chunks (not "one block"). Added the chunk-coverage math and a cross-reference to `docs/pfr-reference.md`. LOCATE latency band tightened from "tens of seconds" to documented numbers. |
| 7.1 / 7.3 / 7.2 | gRPC cancellation reworded as best-effort with safe-point model. New terminal-state vocabulary: `Succeeded`, `Failed`, `CancelledBeforeDispatch`, `CompletedAfterCancel`, `CancellationRejected`, `CompletionUnknown` — extending the Layer 2b dirty-state discipline upward. |
| 3.2 / 7.2 / 9.0 | `ReadService` expanded into an explicit `OpenSession` / `ReadRange` / `CloseSession` lifecycle, symmetric with `WriteService`. Orchestrator owns the read queue and ordering; rem does not batch internally and does not keep tapes loaded after `CloseSession`. New §9.0 documents oRAO deferral — the API is forward-compatible without spec churn now. Layer 3a clarified: no request queue, one LOCATE + read per call. |
| 5.5 / pfr-reference §4 | PFR trim formula corrected for short last chunk. Earlier draft used `chunks_to_read * chunk_size` as the decompressed byte count; that overstates when the read includes a file's naturally-short last chunk and would lead to over-trimming. Fix: subtract `chunk_size` and add `short_last_size = file_size − last_chunk_index * chunk_size` when the last chunk is in the read range. New worked example at `pfr-reference.md` §4.4. |
| 6.2 / 6.3 | §6.3 retitled "Hot-plug integration (Layer 2c)" (was "Layer 2c, planned"). The watcher itself ships; daemon-side wiring of bursts → `refresh()` is Layer 5's job and not yet started. §6.2 reworded so that re-derivation triggers list process start, `Refresh`, and hot-plug — the latter active once Layer 5 consumes the watcher. |
| 9.0 / 9.0a (new) | oRAO contradiction with Layer 3a contract resolved by taking the strict path — if oRAO ever becomes relevant it gets a new explicit `BatchReadRange` RPC, not transparent reorder inside `ReadRange`. Session lifecycle edge cases (client disconnect, idle timeout, daemon restart, found-loaded tape, reservation conflict) enumerated in 9.0a as an implementation-phase checklist. |
| pfr-reference §6.4 | zstd seek-table entry size corrected: 8 or 12 bytes (4+4+optional 4), not 12 or 16. |
| 10 | M2 split into M2a / M2b / M2c. M2a + M2b live-tested on production MSL3040; M2c implementation and live smoke completed on akash 2026-05-18 (daemon-side wiring still pending in Layer 5). |
| (new doc) | `docs/pfr-reference.md` — hardware substrate, LOCATE latency, chunk math, encryption interaction, prior art. Spec v0.3 §5.5 links into it. |
| 1.1 | Reframed purpose around the "filesystem for tape" mechanism analogy. |
| 1.2 | Added "minimal scope" as design priority #2. |
| 1.4 | Sharpened scope boundary list; added explicit non-ownership of cross-tape search, dedup, scheduling, WORM. |
| 2.2 | Renamed "Host environment" → "Host environment and OS portability"; added the OS-portability seam table; macOS explicit non-goal; Windows portability goal. |
| 2.3 | Integration target now references gRPC rather than HTTPS REST. |
| 3.1 | Dropped PostgreSQL from the process model. Daemon is single-writer to disk-resident state. |
| 3.2 | Reworked layering: Layer 3 split into 3a (tape mechanism) + 3b (tape format); Layer 4 renamed from "Catalog" to "Local state"; Layer 5 is gRPC. |
| 3.3 | Rewrote "Persistence" entirely. No database. Disk cache regenerable from tape; audit log is the only durable daemon-owned state. |
| 5 (entire) | On-Tape Format rewritten. Hard split between fixed catalog block (rem's bookkeeping) and pluggable body format. New §5.3 capability tiers (`TapeFormat`, `FileAddressable`, `ByteRangeAddressable`), §5.4 format registry, §5.5 LBA-based seek (PFR mechanics), §5.7 built-in formats (`pax-tar-v1`, `rem-chunked-v1`). |
| 6.1 / 6.3 | Mentioned OS-portability seams; Linux-specific path naming kept but flagged as one implementation behind a trait. |
| 6.4 | Added "Object on tape" and "File within object" to the identifier table. |
| 7 (entire) | API moved from HTTPS REST + SSE to gRPC + bidi streams. Services enumerated. SSE removed. REST gateway listed as a deferrable optional layer. |
| 8.2 | "Append-only audit log" bullet references local-state layer instead of database. "Catalog backup" reworded to "Tape-content recovery without daemon state." |
| 9 (renumbered) | 9.1 → rem-chunked-v1 spec; 9.2 → catalog block schema; 9.6 (new) → cross-tape spans; 9.8 (new) → OS portability beyond Linux. v0.2's 9.1 (manifest schema) absorbed into 9.1 + 9.2. |
| 10 | Roadmap reflects the new layer split (M3a / M3b) and adds M8 (portability seams) + M11 (third-party formats). |
| Glossary | Added FileAddressable, ByteRangeAddressable, TapeFormat, LBA, LOCATE BLOCK, PFR, gRPC, SPTI. Removed SSE. |

---

## Appendix C: Changes from v0.1

(Carried forward from v0.2's Appendix B for historical continuity.
See `docs/spec-v0.2.md` for the full v0.1 → v0.2 changelog.)
