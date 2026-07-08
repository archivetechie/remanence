# Remanence — Tape Library Management Tool for LTO Archives

**Version 0.2 (Draft) — May 2026**
**Archives Team**

> Supersedes `docs/spec-v0.1.docx` (May 2026). v0.1 is retained as a
> historical artifact. This version incorporates corrections and
> clarifications from the Layer 1 build-out, real-hardware capture
> exercise on the production MSL3040, and the Layer 2 design pass —
> all documented in `JOURNAL.md`. Detailed schemas (manifest, API,
> Postgres) remain deferred to follow-on documents.

---

## 1. Introduction

### 1.1 Purpose

Remanence is a tape library management tool for LTO-based archives. It exposes a clean, well-documented API for higher-level archive management systems (such as Dwara v3) to write data to tape, read data from tape, and manage the physical state of the tape library. It is deliberately scoped as a tape subsystem — not an archive management system — and does not own the cross-tier data catalog.

The name refers to magnetic remanence: the property of a ferromagnetic material to retain its magnetisation after the external magnetising field is removed. It is the physical reason data persists on tape when power is disconnected, and the reason tape remains the standard medium for long-term archival storage. The command-line interface is named `rem`.

### 1.2 Why this exists

This project was built for a multi-petabyte tape archive that has historically been managed by an in-house system (dwara v2). A migration to a commercial system (Atempo Miria) has been in progress for an extended period without successful production deployment. Remanence is being developed as a focused, well-engineered alternative to the tape-management portion of that stack — buildable by a small team, defensible on technical merits, and easy to integrate with whatever orchestration layer ultimately governs the archive.

The design priorities, in order:

1. **Correctness and data integrity above all.** This system writes the only copies of irreplaceable data.
2. **Operational legibility.** An operator with the documentation should be able to understand what the system is doing and why.
3. **Format longevity.** Data written today must be readable in thirty years, ideally with standard Unix tools, even if Remanence itself no longer exists.
4. **Ease of integration.** Higher-level systems should find Remanence pleasant to integrate with, in deliberate contrast to existing alternatives.
5. **Security against realistic threats.** The tool must not be a soft target for attackers seeking to disrupt or damage the archive.

### 1.3 What this document is

A high-level specification of architecture and key design decisions. It is not implementation-ready. It captures the choices that have been made and the reasoning behind them so that subsequent detailed design can proceed from a stable foundation. Detailed schemas, APIs, on-tape formats, and protocols are specified in follow-on documents — Layer 2 discovery is already drafted in `docs/layer2-design.md`; manifest and API schemas will follow.

### 1.4 Scope boundaries

**Remanence owns:**
- The state of every tape library partition the host can reach (drives, slots, mailslot, picker).
- Tape-level operations (load, unload, write, read, eject, import, verify).
- Tape-level metadata (per-tape state, MAM, health, write history).
- The on-tape format and any data structures written to tape.
- Hardware health, diagnostics, and drive cleaning workflows.
- Encryption for tape copies, including the key-management integration.

**Remanence does not own:**
- The logical file catalog across all storage tiers.
- Multi-tier orchestration (disk, cloud, tape coordination).
- User-facing archive workflows (ingest, retrieval policy, retention).
- Disk or cloud storage backends.
- End-user authentication.

These are the responsibility of the orchestration layer (Dwara v3 or any equivalent system). Remanence exposes its services via an API; the orchestration layer composes them into archive workflows.

---

## 2. Operating Context

### 2.1 Hardware

Remanence is designed against a heterogeneous fleet of LTO libraries. Primary production targets and their drive generations:

| Library | Drive generations | Notes |
|--|--|--|
| **HPE StoreEver MSL3040** | LTO-9 (primary), LTO-7 | Single chassis (expandable to seven stacked modules / 280 slots). Routinely partitioned into multiple logical libraries — each partition appears as an independent SCSI medium changer (see §6.5). |
| **Overland Storage XL80** | LTO-7, LTO-6 | Legacy library kept in service to broaden the addressable archive fleet. Multiple drive bays; same SCSI command set as the MSL3040 modulo vendor quirks. |

Supported LTO drive generations across the fleet: **LTO-6, LTO-7, LTO-8, LTO-9**.

Drive count and slot count are configurable per deployment. The tool must scale to a fully populated multi-chassis configuration and to a host with multiple libraries attached simultaneously (a single Remanence daemon can manage all of them).

### 2.2 Host environment

Remanence is designed to run on modern Linux distributions with kernel 5.10 or later. Primary development and reference deployment target is Ubuntu 24.04 LTS. RHEL 9, Rocky 9, and AlmaLinux 9 are also supported as deployment targets. CentOS 7 and other end-of-life distributions are explicitly not supported, particularly because LTO-9 first-load calibration interacts poorly with older kernel SCSI driver behavior.

The host runs the Remanence daemon as a long-lived systemd service with appropriate Linux capabilities for SCSI generic device access. No container or VM is required, and none is recommended for production: hardware-managing daemons benefit from direct access to kernel SCSI and udev subsystems without an intervening abstraction.

### 2.3 Integration target

The primary integration target is Dwara v3, an in-development Python-based archive management system that owns the cross-tier catalog. Remanence exposes its functionality via an HTTPS API; Dwara v3 is a client of that API. The API is designed to be usable by any client, not specific to Dwara v3.

A reference Python client library will be provided to make integration straightforward, with a target ergonomics of "common operations expressible in under ten lines of code."

### 2.4 Coexistence with other tape-handling software

Remanence will routinely operate on hosts where other software *also* talks to the same SCSI hardware — either a different Remanence-managed partition, or a different tape-management tool entirely (for example, an existing in-house tool managing a different LTO generation through a different partition of the same chassis). Discovery returns a complete view of every reachable library and partition, but operations are gated on explicit per-partition opt-in (see §6.5 and §8.2). Remanence never operates on a partition it has not been told it owns.

---

## 3. Architecture

### 3.1 Process model

Remanence runs as a single long-lived daemon process on a dedicated host. **A single daemon manages every logical library the host can reach and the operator has configured it to own.** A logical library is the operational unit: one SCSI medium changer with its own drives, slots, and import/export ports. Whether two logical libraries happen to share a physical chassis is irrelevant to Remanence — there are no SCSI operations that cross logical-library boundaries, and the daemon treats each one independently. The same uniformity applies regardless of vendor (HPE MSL3040, Overland XL80, QuadStor VTL, …) and regardless of how the underlying hardware exposes itself (a single unpartitioned library, a chassis configured as several "partitions" each presenting itself as a logical library, multiple physically separate boxes, or any mix).

The daemon exposes an HTTPS API for clients and maintains a persistent catalog of tape-level state in a PostgreSQL database.

The daemon is built in Rust. The choice is driven by: memory safety in code that constructs and parses binary SCSI structures; clean integration with Linux kernel subsystems (sg, udev) via mature crates; suitability for long-running concurrent operations via the tokio async runtime; and single static-binary deployment with minimal runtime dependencies.

### 3.2 Layering

The codebase is structured in clear layers, with each layer testable independently:

- **Layer 1: SCSI core** (`crates/remanence-scsi`). Pure SCSI command construction, ioctl invocation, and response parsing. INQUIRY, VPD pages, READ ELEMENT STATUS, MOVE MEDIUM, LOG SENSE, READ/WRITE ATTRIBUTE, SECURITY PROTOCOL IN/OUT, and tape-specific commands (LOAD, UNLOAD, REWIND, LOCATE, READ POSITION, SPACE, WRITE FILEMARKS, READ, WRITE). No business logic, no I/O policy, no concurrency. Testable in isolation against captured response fixtures. **Status: in flight. INQUIRY, VPD 0x80, READ ELEMENT STATUS with DVCID landed. 32/32 tests pass against both QuadStor and real-MSL3040 fixtures.**
- **Layer 2: Library runtime model** (`crates/remanence-library`). The in-memory model of library topology, drive states, and tape inventory. Subscribes to udev events for hot-plug awareness. Performs the join-by-serial logic that maps library-reported drive bays to host-side device nodes (§6). Provides a clean abstraction over Layer 1 for higher layers: "the drive whose serial is X" rather than `/dev/sg5`. **Status: discovery side is designed in `docs/layer2-design.md`; implementation pending.**
- **Layer 3: Tape operations** (`crates/remanence-tape`). The vocabulary of tape-level operations: write session (open, append object, close), read session, move, eject, import, verify. Handles streaming data between the API layer and the tape, including buffering, rate matching, and progress reporting. Implements the on-tape format (§5).
- **Layer 4: Catalog** (`crates/remanence-catalog`). Persistent state in PostgreSQL: tapes, objects on tapes, drives, write sessions, operations, audit log. Schema migrations managed via sqlx. The catalog is authoritative for "what Remanence knows about its hardware and tapes," but does not own logical file identity — that belongs to the orchestration layer.
- **Layer 5: API** (`crates/remanence-api`). HTTPS server with mTLS authentication. RESTful resource model plus a Server-Sent Events stream for asynchronous notifications. Translates external requests into Layer 3 operations and Layer 4 catalog queries. Enforces authorization and rate limiting. Emits structured audit log entries for every request.

### 3.3 Persistence

Catalog data lives in PostgreSQL 16 or later. The catalog is small (gigabytes, not terabytes) and query-intensive. Backups are taken via streaming replication to a standby plus daily logical dumps shipped offsite. WAL archiving supports point-in-time recovery.

The catalog is not the only persistent store of tape contents: every tape is itself self-describing (§5). The catalog is a fast, queryable mirror of information that is also written durably to the tapes themselves. This redundancy is intentional: if the catalog is lost, the tapes still describe themselves.

---

## 4. Scope and the Remanence / Orchestrator Boundary

### 4.1 What "tape subsystem" means

The fundamental design decision is that **Remanence answers tape questions; the orchestration layer answers archive questions.** This boundary clarifies many decisions that would otherwise be murky.

| Remanence answers | Orchestrator answers |
|--|--|
| What is on tape `L91234L9`? | Where are all copies of `clip_001.mxf`? |
| Which tapes are in the library? | Which files have insufficient redundancy? |
| What is the health of drive bay 3? | What is our total archive size? |
| Write this byte stream to tape, return location. | Ensure this file has the policy-required copies. |
| Read the object at this position on this tape. | Restore this file from the fastest available source. |
| Which tapes need replacement? | Which files should migrate from tape to disk? |

### 4.2 The caller-object-id pattern

When the orchestrator asks Remanence to write data, it supplies a **caller object identifier** (an opaque string) and optional **caller metadata** (an opaque JSON document). Remanence stores both as part of its record of that object on tape, and writes them into the on-tape manifest alongside the data. Remanence does not interpret the caller object id; the orchestrator uses it to tie tape-resident objects back to its own catalog of files.

This is the integration seam. The orchestrator knows what its identifiers mean and how to map them to logical files. Remanence simply preserves them faithfully.

### 4.3 Reconciliation

The orchestrator periodically reconciles its catalog against Remanence: it queries Remanence for the contents of each tape and confirms that its own records agree. Discrepancies surface either bugs or out-of-band events (operator intervention, hardware failure) and are flagged for investigation. Remanence exposes the queries needed for this reconciliation as part of its standard API; reconciliation is not a special-case workflow.

---

## 5. On-Tape Format

(Sketch retained from v0.1; detailed JSON schemas deferred to a follow-on Layer 3 design doc.)

### 5.1 TAR, not LTFS

Remanence writes POSIX pax-format tar archives to tape. LTFS is explicitly rejected as the on-tape format. The reasoning:

- Tape is a sequential medium. LTFS presents it as random-access, which encourages access patterns the medium does not gracefully support.
- LTFS state corruption from improper unmount is a recurring operational problem, mitigated by recovery tools that should not be necessary in the first place.
- Cross-vendor LTFS interoperability is weaker than commonly claimed, particularly for encrypted or near-full tapes.
- TAR has forty-five years of stable, ubiquitous tooling. A tar tape written today will be readable in 2070 with any Unix-like system's `tar` command, with no specialized software.
- POSIX pax format supports arbitrarily long filenames, extended headers for structured metadata, and proper Unicode — eliminating the historical reasons to prefer LTFS for filename flexibility.

This is a considered choice, not a default. The cost is forgoing the "mount the tape as a filesystem" pattern; the benefit is a dramatically simpler, more durable, more portable format.

### 5.2 Tape layout

Each tape contains a sequence of tar archives separated by tape file marks:

```
[BOT]
  File mark 0: Bootstrap tar archive (tape identity, schema documentation)
  File mark 1: Object 1 (tar archive with manifest + data files)
  File mark 2: Object 2
  …
  File mark N: Object N
  File mark N+1: Periodic index (written every K objects or T hours)
  File mark N+2: Object N+1
  …
  File mark M: Final index (at tape close, written twice for redundancy)
[EOD]
```

### 5.3 The bootstrap archive (file mark 0)

Every tape begins with a small tar archive at file mark 0 containing tape identity and format documentation. This archive is small enough to read in seconds and is intentionally self-contained — it tells a future reader, with no other context, what this tape is and how to interpret what follows.

Contents:
- `SCHEMA.md` — human-readable description of the on-tape format. Identical (within a major schema version) across all tapes.
- `tape-info.json` — tape barcode, generation, pool, encryption status, schema version, software version that wrote this tape, write start timestamp.
- `SCHEMA.json` — machine-readable JSON schema for the manifest format used in subsequent object archives.

### 5.4 Object archives

Each object written to tape is a single complete tar archive containing both the data files and a manifest. The archive is self-contained: extracting it with standard tar tools yields the data files and a `manifest.json` describing them, with no dependency on external state.

Layout within an object archive:

```
manifest.json                       — object-level metadata + file-level catalog
files/relative/path/file1.mxf       — actual data files
files/relative/path/file2.wav
…
```

The manifest contains, at minimum: object id, caller-supplied object id, write timestamp, tape barcode, file number on tape, total size, encryption parameters (including the wrapped data encryption key if applicable), per-file entries with path / size / SHA-256 / modification time, and caller-supplied opaque metadata. A formal JSON schema for this manifest is part of the format specification.

### 5.5 Periodic and final indexes

At regular intervals during writing — every K objects or every T hours of write time, whichever comes first — Remanence appends a periodic index. This is a small tar archive containing a single `tape-index.json` file with a cumulative index of all objects written so far. Periodic indexes serve two purposes:

- **Recovery checkpoint:** if a tape is interrupted (operator pull, power loss, drive failure), the most recent periodic index gives a complete picture of what was successfully written, without requiring a full-tape scan.
- **Fast catalog reconstruction:** rebuilding catalog state from a tape requires reading only the latest index plus any object manifests written since the index, not the entire tape contents.

At tape close, a final index is written (twice, for redundancy near end-of-data) with the `is_final` flag set. The MAM is updated with the position of the final index for fast access on subsequent mounts.

### 5.6 MAM (Medium Auxiliary Memory) usage

Every LTO cartridge contains a small writable memory chip accessible via SCSI READ ATTRIBUTE / WRITE ATTRIBUTE commands. Remanence uses this for:

- Latest index position (file number and block id) for fast catalog access.
- Total objects written and total bytes written.
- Last write timestamp.
- Tape state flags (writing in progress, finalized, suspect, retired).
- Schema version of the on-tape format.

MAM is not used as primary storage for any data — it is a hint, not a source of truth. The authoritative state is on the tape itself (in the indexes and manifests) and in the catalog. MAM speeds up common operations and provides a useful breadcrumb after unexpected events.

### 5.7 The "catalog on tape" property

Taken together, the bootstrap archive, per-object manifests, periodic indexes, and final index form a complete self-describing record of the tape. The catalog database is a fast index for queries, but the tape itself is the durable record. If the catalog is lost entirely, every tape can be walked and its full contents reconstructed using only standard `tar` tooling and the documented JSON schemas.

A **catalog dump tape** — a tape written periodically containing a full export of the Remanence catalog state — provides an additional recovery shortcut. With the most recent catalog dump tape plus the periodic indexes on subsequently-written tapes, catalog state can be fully reconstructed without a complete tape-by-tape walk.

---

## 6. Hardware Abstraction

### 6.1 The drive enumeration problem

A foundational operational problem with SCSI tape libraries is that the kernel-assigned device nodes (`/dev/sg*`, `/dev/nst*`) do not stably correspond to physical drive bays in the library. After a reboot, an HBA rescan, or a drive replacement, the same physical drive may appear under a different device node. Software that hard-codes device paths breaks when this happens.

### 6.2 The join-by-serial solution (empirically grounded)

Remanence never persists kernel device paths. On every startup and every relevant udev event it derives a fresh bay-to-device mapping by issuing two cheap queries and joining their results by serial number:

1. **From the library**, READ ELEMENT STATUS with both the `DVCID` and `CurData` bits set (CDB byte 6 = `0x03`), `element_type=4` (Data Transfer Elements). The response includes a 34-byte identifier descriptor per drive containing the drive's vendor, product, and serial number.
2. **From the kernel**, enumerate `/dev/sg*`, issue INQUIRY + VPD page 0x80 to each tape device, and read its serial.

Joining these two mappings by serial number yields the runtime correspondence between library bay and kernel device.

> **Why both bits?** The primary discovery request sets DVCID and CurData together so the changer can return drive identifiers with cached element state in one read-only probe. This combination is verified against production MSL3040 firmware 3350 and QuadStor VTL fixtures. Discovery still probes the alternate CurData polarity as a compatibility fallback; older DVCID-alone notes predate the corrected CDB bit mapping and are not treated as hardware evidence.

#### 6.2.1 Fallback when DVCID is unavailable

For libraries that don't honor the DVCID+CurData combination (vendor firmware we have not yet tested — including the Overland XL80 — may behave differently), Remanence's discovery falls back through:

1. Retry with `element_type=4` (drives-only) if the all-types form was used.
2. Retry with CurData inverted.
3. Final fallback: derive the bay-to-serial mapping from the SCSI bus topology — drives on the same `host:channel` as the changer, sorted by SCSI ID — and emit a discovery warning that the result depends on a vendor-specific convention. Discovery is not failed by this; the operator is informed.

The discovery API surfaces these fallbacks as warnings in the returned report (see `docs/layer2-design.md` §5.1), never silently substituted.

### 6.3 udev integration

Remanence subscribes to udev events on the `scsi_generic` and `scsi_tape` subsystems and re-runs discovery on add, remove, and change events. The model is "re-derive on event," not "incrementally update on event" — the cost of full re-enumeration is sub-second even for fully-populated multi-chassis libraries, and the simpler model avoids edge cases where multiple things change at once.

Supplementary udev rules install stable symlinks at `/dev/tape/by-serial/{serial}` for operator convenience and as a fallback for ad-hoc debugging. These symlinks are not used by Remanence itself; the runtime always resolves device paths via its own enumeration.

### 6.4 Identifiers

Throughout Remanence's code and persistent state:

| Entity | Stable identity | Source |
|--|--|--|
| Library | Library serial (e.g. `DEC418146K_LL02`) | Changer VPD 0x80 |
| Drive | Drive serial number | Drive VPD 0x80 / inline DVCID identifier in RES |
| Drive bay | `(library_serial, element_address)` | Composite — stable across drive swaps |
| Slot | `(library_serial, element_address)` | RES element address |
| Cartridge | Barcode (volume tag) | RES descriptor / library's barcode reader |
| `/dev/sgN`, SCSI ID, sysfs path | **not stable** | Linux enumeration order; treat as labels-of-the-day |

No identifier in Remanence's persistent state is platform-specific or installation-specific. A catalog can be moved between hosts without remapping.

The changer's VPD 0x83 NAA descriptor (a World-Wide Name) is also captured and recorded as an informational attribute of the library — useful for diagnostics and for operator UX that wants to highlight "these three libraries you're looking at all live in the same chassis." No operational logic depends on it; it never gates a command or selects a code path. If the library doesn't return a NAA descriptor at all, the field is simply `None`.

### 6.5 One model for partitioned and physically-separate libraries

Some enterprise libraries — the MSL3040 included — support **partitioning**: a single physical chassis is configured by the operator to present itself as several independent SCSI medium changers, each with a subset of drives, slots, and IE ports. From Remanence's point of view this is indistinguishable from having several physically separate libraries on the same host: each one is a logical library, identified by its own VPD 0x80 serial, addressable by its own `/dev/sgN`, and operated on independently. Partitioning at the chassis level is an operator concern (configured via the chassis's front panel / web UI) — Remanence inherits whatever logical libraries the host happens to expose and does not need to know that some of them happen to share hardware.

This deliberately keeps the model simple. The deployments Remanence will see — a single unpartitioned library; a single chassis partitioned into N logical libraries; M physically separate unpartitioned libraries; any mix — all reduce to the same thing: **a flat list of logical libraries.**

Two consequences worth being explicit about:

- **Drive bays are assigned to logical libraries by the library/chassis firmware**; the assignment is *not* derivable from SCSI bus topology. The physical HBAs on a host can each see drives from multiple logical libraries (a single partitioned chassis routes its drive bays across both cables). Discovery must use the DVCID-RES path (§6.2) to learn which drives belong to which library.
- **Each logical library is operated on independently.** Operations are gated on explicit per-library opt-in: Remanence will not issue state-changing commands against any library not on its allowlist (§8.2). This is what lets Remanence safely share a host with another tape-handling tool — including running on a partitioned chassis where some logical libraries are owned by a different system.

The Layer 2 design doc (`docs/layer2-design.md`) covers the discovery algorithm and value types.

---

## 7. External API

### 7.1 Transport

Remanence exposes its API over HTTPS on a configurable TCP port (default 8443). TLS 1.3 only. Mutual TLS authentication is required: clients must present a valid certificate issued by the Remanence deployment's private CA. Plain HTTP is not supported in production deployments; an unauthenticated local development mode is available for testing.

The choice of mTLS over bearer tokens is deliberate. mTLS gives cryptographic authentication with no shared secret to leak, supports per-client revocation without coordination, and integrates naturally with operational best practices (short-lived certs, automated rotation via step-ca or equivalent).

### 7.2 Resource model

A small, opinionated REST surface. Major resources:

- **Libraries** — logical libraries the daemon manages, each with its own drives, slots, and IE ports. The operational unit of the API: tape and drive endpoints are addressed by library serial. A deployment can manage any number from any supported vendor, including multiple logical libraries that happen to share a physical chassis (see §6.5).
- **Drives** — drives in libraries, with health and operational state.
- **Tapes** — cartridges known to Remanence, with current physical location and state.
- **Objects** — data written to tapes, addressable by tape barcode and file number.
- **Write sessions** — open contexts for writing one or more objects to a tape.
- **Read requests** — requests to retrieve object contents from a tape.
- **Operations** — long-running tasks (moves, writes, reads, verifies) with status and progress.

All write-initiating endpoints accept an `Idempotency-Key` header (a UUID supplied by the client). If a client retries a request after a network failure, Remanence returns the original operation rather than creating a duplicate. This is critical for tape operations, where "did the move actually happen?" is a question with severe consequences.

### 7.3 Long-running operations

Tape operations span seconds to hours (an LTO-9 first-load calibration alone can take up to two hours). Synchronous request-response over HTTP is not appropriate for the time scales involved. The API uses an explicit operation-id pattern:

- A POST that initiates work returns `202 Accepted` with an operation id and lifecycle state (`pending`, `running`, `succeeded`, `failed`, `cancelled`).
- Clients may poll the operation endpoint for status, or subscribe to the event stream for push notifications, or both. The API tolerates either access pattern.
- Progress reporting (bytes written, current step, ETA) is provided on a best-effort basis for operations where it is meaningful.

### 7.4 Events

Remanence provides a Server-Sent Events stream at a single endpoint. Events include operation lifecycle transitions, drive state changes, tape load/unload, alert conditions (TapeAlert flags), and mailslot activity. Clients reconnect with the standard `Last-Event-ID` header to resume from a known position; no events are lost in normal operation.

SSE is preferred over WebSockets because: it is plain HTTP and inherits all HTTP tooling (auth, logging, debugging); it is strictly server-to-client, matching the actual event flow; reconnection is built into the protocol; and Python client support is mature.

### 7.5 Authorization

Authorization derives from the client certificate. Roles encoded in the certificate subject determine the permission set:

- `orchestrator` — full access to write, read, and library management operations. Typically held by the orchestration layer (Dwara v3).
- `operator` — administrative access including library management, drive cleaning, tape import/export, but not write operations.
- `readonly` — query access to library state, tape contents, operation history. Suitable for monitoring and reporting tools.
- `admin` — additionally authorised for sensitive operations (read-only mode toggle, emergency stop, cert revocation acknowledgement).

Role-to-permission mapping is explicit and enumerable. There is no general-purpose policy language at the authorization layer; the permission set is small and the cost of generality exceeds its benefit.

#### 7.5.1 Library scoping

Authorization is scoped not just by role but by library. A certificate's subject may include a `libraries` attribute restricting the client to a specific set of library serials. A client without a library restriction can act on any library the daemon manages; a restricted client targeting a library outside its allowed set receives `403 Forbidden`. Lets multiple orchestrators share a single Remanence deployment safely.

### 7.6 Reference client library

A Python client library (`remanence-client`, with a top-level package importable as `remanence`) is provided as a first-class deliverable. It wraps the REST API and the SSE stream, presents an ergonomic synchronous and asynchronous interface, and serves as both a reference implementation and a "five-minute integration" path for the orchestration layer. The library has minimal dependencies (`httpx`, `httpx-sse`, `pydantic`) and is generated and hand-tuned from an OpenAPI 3.1 specification of the wire protocol.

---

## 8. Security

### 8.1 Threat model

Remanence assumes a hostile network. Even on a "trusted" LAN, the design assumes that an attacker may eventually obtain network access, may possess credentials of compromised internal users, or may attempt to disrupt or damage the archive (for example via ransomware). Defenses are layered accordingly.

**Explicitly in scope:**
- Network-based attackers attempting to disrupt or damage tape contents via the API.
- Compromised credentials of legitimate clients.
- Insider threats with limited access attempting to escalate or do damage within their scope.
- Ransomware reaching the daemon host and attempting to encrypt or delete archive data.
- Accidental or malicious tampering with the audit log or catalog database.

**Explicitly out of scope:**
- Physical access to the library, drives, or tapes.
- Root-level compromise of the daemon host (assumed game-over).
- Supply chain attacks against the build toolchain.
- Side-channel attacks against the host hardware.

### 8.2 Defense in depth

- **Cryptographic authentication.** mTLS for every API client, every operator, every internal service. No bearer tokens, no shared secrets.
- **Least privilege.** The daemon runs as an unprivileged user with only the Linux capabilities and device permissions it strictly needs. The database user has only the schema permissions it strictly needs.
- **Process hardening.** systemd unit applies `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`, `RestrictAddressFamilies`, and related options.
- **Library allowlist.** The daemon configuration carries an explicit list of library serials it is allowed to issue state-changing commands against. Discovery surfaces every reachable library; the daemon refuses to operate on a library not on the allowlist. This is the operational guarantee that lets Remanence run on a shared host without risk of touching another tool's library.
- **Append-only audit log.** Every authenticated request, every state change, every cert action is logged with a hash-chained entry whose hash is periodically checkpointed offsite. Tampering is detectable.
- **Anomaly thresholds.** Destructive operations beyond normal rates require explicit override. An attacker scripting bulk destruction trips visible rate limits.
- **No destructive primitives.** There is no API endpoint that destroys data. Tapes can be retired, exported, and scratched only by deliberate operator action with explicit confirmation.
- **Read-only and emergency stop modes.** A single privileged operation puts Remanence into a state where it serves reads and refuses writes/moves. Useful for maintenance and indispensable during incident response.
- **Catalog backup.** Daily encrypted backups of the catalog database, shipped offsite. Combined with the per-tape self-description, catalog recovery is always possible.

### 8.3 Certificate management

Certificates are issued by a private CA managed via `step-ca` or equivalent. Standard practices apply: short certificate lifetimes (90 days), automated renewal, certificate revocation lists or OCSP for revocation. The CA itself is offline or air-gapped from the daemon host, with its signing key held under appropriate physical and access controls.

### 8.4 Encryption of tape contents

Remanence supports per-tape encryption using LTO hardware encryption (AES-256-GCM) via the SCSI SECURITY PROTOCOL IN/OUT commands. Encryption is configured per write session — the orchestration layer indicates whether a given tape should be encrypted and provides a reference to the key material.

#### 8.4.1 Key architecture

Remanence uses a two-tier key model:

- A **Key Encryption Key (KEK)** is held outside Remanence in a hardware-backed key store (typically a YubiKey PIV applet, with offline backup copies under operator control). The KEK never leaves the hardware.
- **Per-tape Data Encryption Keys (DEKs)** are generated at write time. The DEK is sent to the drive via SECURITY PROTOCOL OUT for the duration of the session, wrapped by the KEK for catalog storage, and forgotten by the drive at unload.

To read an encrypted tape, the catalog's wrapped DEK is retrieved, unwrapped via KEK access (an explicit, audited operation), and sent to the drive. This decouples key recovery from any single piece of software, host, or service.

#### 8.4.2 Key recovery

Threshold cryptography (Shamir's secret sharing) is used to split the KEK across multiple custodians, with a threshold smaller than the total — e.g., three custodians, any two of whom can reconstitute the KEK. This protects against both single-point loss (one custodian unavailable) and single-point compromise (one custodian malicious). A documented, tested, annually-exercised recovery procedure ensures that future operators can decrypt tapes even if no one currently familiar with the system remains.

#### 8.4.3 What is encrypted

Remanence supports a mixed model: the orchestration layer decides on a per-tape basis whether to encrypt. The typical deployment writes three copies of archive data with one copy encrypted (defense against scenarios where unencrypted copies are physically compromised — theft, legal seizure, insider exfiltration). All three copies use the same Remanence mechanisms; the difference is purely in the encryption parameters supplied at write time.

---

## 9. Open Questions and Deferred Decisions

Several design questions are deliberately deferred to subsequent specifications, either because they require more discussion, or because they are best decided once the foundational layers are working.

### 9.1 Specific manifest schema

The structure of the per-object manifest and the per-tape index is sketched in §5 but not formally specified. A detailed JSON schema, with stable field names, versioning conventions, and migration policy, is required before any tape is written in anger. **Status: still open.** Best decided when Layer 3 implementation begins and we have a clearer picture of what fields the orchestrator actually wants.

### 9.2 SCSI reservation strategy

How Remanence uses PERSISTENT RESERVE IN/OUT for drives and the changer. **Status: leaning toward "minimal."** Logical-library isolation in the firmware (§6.5) handles separation between Remanence and any other tool sharing the same chassis, so changer-level reservations are not necessary in the typical deployment. Per-drive PERSISTENT RESERVE during an active write or read session — held for the session duration, released on close or timeout — provides robustness against concurrent access by a misbehaving second initiator. The exact reservation parameters will be settled with the first Layer 3 write-session implementation.

### 9.3 Write verification policy

Should every object written be immediately verified by reading back and checksumming? Always, sometimes, never? Decision likely depends on observed error rates with the specific MSL3040 + LTO-9 combination. **Status: still open.**

### 9.4 Multi-library and mixed-vendor deployments

A single Remanence daemon manages every reachable logical library (§3.1, §6.5). The architecture supports this natively — every operation keys off `(library_serial, …)`. What remains open is operational:

- **Cross-library moves are impossible at the SCSI level.** A cartridge in one logical library cannot be moved into another's slot space without operator intervention (physical pull and import). This applies whether the two libraries are partitions of one chassis or physically separate boxes. The API surfaces this as a discoverable property rather than failing on attempts.
- **Tape-pool semantics across libraries** — does a "pool" of tapes belong to one library, or can it span libraries? Default leaning: pools are library-scoped; the orchestrator coordinates across-library moves explicitly.
- **Identifier disambiguation in operator UX** — when a human says "the library," they probably mean a specific one. The CLI and any TUI should prompt for library serial whenever the context is ambiguous.

These are operator-facing decisions to spell out in the operator documentation, not in this spec.

### 9.5 Catalog dump tape cadence and format

How often a catalog dump tape is written, what format it uses, how it is distinguished from regular tapes, how its existence is recorded for recovery use. **Status: still open.**

### 9.6 Operator UI

Remanence exposes an API but not a user interface. Whether to ship a reference TUI (`ratatui`-based) and/or web UI as part of the project, or to leave that to the orchestration layer, is an open question. A TUI is straightforward to build and dramatically improves the day-one experience for operators. **Status: still open.**

### 9.7 Distribution and licensing

**Resolved (v0.2):** Remanence is licensed under **AGPL-3.0-or-later**. Source repository is private during initial development; will be made public once the core layers are functional. Packaging strategy (deb/rpm packages, container images, source-only) is still open.

### 9.8 Vendor coverage beyond HPE

The Layer 1 fixtures are exhaustively covered for the HPE MSL3040 / HPE Ultrium 9 / HP Ultrium 7. Coverage for the Overland Storage XL80 and its LTO-6 / LTO-7 drives is **pending capture**. The same capture script (`scripts/capture-msl3040.sh`) applies; the in-tree fixture corpus and the parser tests will grow accordingly. Vendor-specific quirks discovered along the way are recorded in the relevant Layer 1 and Layer 2 design notes.

---

## 10. Implementation Roadmap

A staged plan that produces working, testable artifacts at each step. Each milestone is independently demonstrable.

| Milestone | Description | Status (May 2026) |
|--|--|--|
| M1 — SCSI core | Layer 1 in Rust: INQUIRY, VPD pages, READ ELEMENT STATUS, MOVE MEDIUM, LOG SENSE. Unit tests against captured fixtures. | **Partial — 32/32 tests pass; INQUIRY, VPD 0x80, RES with DVCID landed. LOG SENSE / MOVE MEDIUM pending.** |
| M2 — Library runtime model | Layer 2 with udev integration; join-by-serial verified against QuadStor and real hardware. Demonstrable: a CLI that prints live topology. | **Designed (`docs/layer2-design.md`). The `topology` example is the proof-of-concept; production-grade discovery code pending.** |
| M3 — Tape read/write primitives | LOAD, REWIND, LOCATE, WRITE, READ, WRITE FILEMARKS, READ POSITION. Tar reading/writing at the byte level. | Not started. |
| M4 — On-tape format | Bootstrap, per-object manifests, periodic indexes, MAM usage. Final JSON schemas. End-to-end: write → eject → reload → walk → recover. | Not started. |
| M5 — Catalog | PostgreSQL schema, sqlx, migrations. Reconciliation tooling. | Not started. |
| M6 — API | HTTPS / mTLS server, REST + SSE, idempotency keys, audit log. Python client library. | Not started. |
| M7 — Encryption | SECURITY PROTOCOL IN/OUT. YubiKey integration. Key wrapping / recovery. | Not started. |
| M8 — Operational hardening | systemd unit, anomaly thresholds, rate limiting, read-only mode, emergency stop, catalog backup automation. | Not started. |
| M9 — Operator UI (optional) | ratatui TUI and/or web UI. | Not started. |
| M10 — Production readiness | Documented procedures, recovery drills, deployment automation, Prometheus metrics, tested upgrade path. | Not started. |

Milestones 1-4 are sufficient for a useful tool. Milestones 5-8 are required for any production deployment. Milestones 9-10 turn the production-ready tool into a maintainable, supportable system.

---

## Appendix A: Glossary

| Term | Definition |
|--|--|
| **Barcode** | The human- and machine-readable label affixed to a tape cartridge; primary identifier for the tape. |
| **BOT / EOT** | Beginning of Tape / End of Tape — physical reference positions on the tape medium. |
| **CDB** | Command Descriptor Block — the binary structure that defines a SCSI command. |
| **CurData** | "Current Data" bit in the READ ELEMENT STATUS CDB (byte 6 bit 1). Requests cached element state without device motion. Used alongside DVCID by the primary Remanence discovery probe. |
| **DEK** | Data Encryption Key — a per-tape symmetric key used for hardware encryption of tape contents. |
| **DVCID** | Device Identifier bit in the READ ELEMENT STATUS CDB (byte 6 bit 0). When honored, the changer returns each Data Transfer Element's vendor, product, and serial inline in the descriptor. |
| **EOD** | End of Data — the position on tape after the last written data. |
| **File mark** | A tape-internal marker separating distinct files or records. |
| **KEK** | Key Encryption Key — a master key used to wrap and unwrap per-tape Data Encryption Keys. |
| **LTFS** | Linear Tape File System — standardised as ISO/IEC 20919. **Explicitly not used** by Remanence. |
| **LTO** | Linear Tape-Open — the dominant open standard for high-capacity tape storage. |
| **MAM** | Medium Auxiliary Memory — a small writable memory chip inside each tape cartridge, accessible via SCSI READ/WRITE ATTRIBUTE. |
| **mTLS** | Mutual TLS — TLS in which both client and server present and verify certificates. |
| **Partitioning** | A vendor feature that lets one physical chassis be split into several logical libraries at the front panel / web UI. From Remanence's point of view, each resulting logical library is simply a library. |
| **Pax format** | A POSIX-standardised extension of the tar archive format, supporting long filenames and extended metadata. |
| **Picker** | The robotic mechanism in a tape library that moves cartridges between slots and drives. |
| **SCSI** | Small Computer System Interface — the command protocol for tape drives and libraries, regardless of physical transport. |
| **SMC-3** | SCSI Media Changer-3 — the standard defining the command set for tape libraries. |
| **SPC-5 / SSC-5** | SCSI Primary Commands / SCSI Stream Commands — the standards for SCSI devices generally and tape drives specifically. |
| **SSE** | Server-Sent Events — a simple protocol for server-to-client event streaming over HTTP. |
| **udev** | The Linux subsystem that manages device nodes and exposes hot-plug events. |
| **VPD** | Vital Product Data — a SCSI mechanism for retrieving structured device information including serial numbers. |
| **WWN (NAA)** | World-Wide Name in IEEE NAA format. Globally unique 64-bit identifier carried in VPD page 0x83 NAA descriptors; used as the stable chassis identity. |

---

## Appendix B: Changes from v0.1

| § | Change |
|--|--|
| 2.1 | Added Overland Storage XL80 as a primary production target. Listed supported LTO drive generations (6/7/8/9) explicitly. Removed the framing of LTO-7 as out-of-scope legacy hardware. |
| 2.4 (new) | Added a coexistence section spelling out that Remanence shares hosts with other tape-handling software safely via the partition allowlist. |
| 3.1 | Explicit unified model for partitioned and physically-separate libraries managed by a single daemon. |
| 3.2 | Layer-1 status updated to reflect in-flight implementation (32 tests passing). Cross-reference to `docs/layer2-design.md`. |
| 6.2 | Rewrote the join-by-serial mechanism to reflect the empirical finding that HPE firmware requires DVCID *and* CurData together. Added §6.2.1 with the fallback ladder for libraries that don't honor DVCID. |
| 6.4 | Replaced the "drives by serial / libraries by serial / slots by element address" list with a complete identity table that includes chassis WWN and partition serial — the v0.1 list pre-dated the partitioning model. |
| 6.5 (new) | One model for partitioned and physically-separate libraries: both reduce to a flat list of logical libraries. Remanence does not model "chassis" as a first-class concept. |
| 7.2 | Library is the operational unit of the API; no chassis or partition resources. |
| 7.5.1 (new) | Authorization scoping by library serial. |
| 8.2 | Added the library allowlist as a defense-in-depth layer. |
| 9.2 | Reservation strategy leaning toward "minimal" rather than "TBD" — the answer is now visible. |
| 9.4 | Multi-library reframed to cover any mix of partitioned, unpartitioned, and physically-separate libraries — all the same shape. |
| 9.7 | License resolved to AGPL-3.0-or-later. |
| 9.8 (new) | Vendor coverage beyond HPE listed as pending. |
| 10 | Roadmap table updated with milestone statuses. |
| Glossary | Added CurData, Partition, WWN (NAA). Clarified DVCID. |
