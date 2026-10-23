# Why Remanence?

<!-- code-anchor: none -->

This document compares Remanence with the other ways of putting data on LTO tape
and explains the trade-offs in operator terms. It is for someone choosing between
Remanence and an alternative. The format commitments themselves are in the published
[REM-OBJECT Core Format 1.0](../specs/publication/rem-object-core-1-specification.md),
[REM-ENCRYPT 1.0](../specs/publication/rem-encrypt-1-specification.md), and
[REM-PARITY 1.0](../specs/publication/rem-parity-1-specification.md)
specifications; this document explains the reasoning behind those commitments by
comparing Remanence with the alternatives. The maturity of the software and the
status of the specifications are described at the end, under
[Status and maturity](#status-and-maturity).

---

## What Remanence is, in one sentence

Remanence is the tape-mechanics layer for LTO archives: a mechanism for reading,
writing, locating, and accounting for data on tape, with the safety properties an
archival operator needs.

The mental model is a filesystem driver. It does not decide what to archive, when
to archive, or for how long to keep things. It puts bytes on physical media in a
way that is durable, recoverable, and legible to an operator. Higher-level archive
workflows (retention policy, cross-tier orchestration, deduplication, job
scheduling, user-facing UI) are out of scope and belong to the orchestrator that
calls Remanence's API.

The small scope is the main difference between Remanence and the alternatives. The alternatives fall into four categories, and each has a weakness for
the archival use case.

---

## The four categories of alternative

| Category | Examples | Strength | Weakness for long-term archives |
|--|--|--|--|
| Raw SCSI utilities | `mt`, `mtx`, `sg_*` from sg3-utils | Universal, scriptable, no daemon | No state model, no safety properties, no compositional reasoning |
| Tape-as-filesystem | LTFS (any implementation) | "Mount the tape and `cp`" | Treats tape as random-access; fragile under interrupted writes; inter-vendor compatibility is weaker than claimed |
| Full backup suites | Bacula, Bareos, BackupExec | Complete workflow out of the box | They do everything, so you inherit everything: their catalog DB, their retention policy, their scheduler, their concept of "job" |
| Proprietary archive systems | Atempo Miria, IBM Spectrum, Veritas NetBackup | Vendor-supported "complete solution" | Closed format, vendor lock-in, opaque failure modes, license cost that compounds over the years |

Remanence is smaller than every category above, on purpose.

---

## At a glance

| Capability | Remanence | `mt`+`mtx` | LTFS | Bacula/Bareos | Proprietary |
|---|:---:|:---:|:---:|:---:|:---:|
| Stable identity (drive ↔ host across reboots) | by serial | by `/dev/sgN` | usually | usually | yes |
| Automatic library ↔ drive join | yes | manual | n/a | partial | yes |
| Per-library allowlist (partitioned-chassis safety) | yes | no | no | via storage daemon | yes |
| Phase-aware error reporting on composed ops | yes | no | n/a | partial | varies |
| Completion-unknown handling (transport-error semantics) | yes (`dirty` state machine) | no | no | partial | opaque |
| Per-operation-class SCSI timeouts | yes | no (single global) | partial | partial | varies |
| Hot-plug aware (re-derive on udev events, the Linux device events) | yes | no | n/a | depends | yes |
| Unit-testable without hardware | yes (captured fixtures and virtual media) | no | no | partial | no |
| Data readable in 10 to 20 years with standard tools | yes (plaintext REM-OBJECT is constrained pax tar, the POSIX tar format) | n/a (raw bytes) | only if LTFS persists | suite-specific | proprietary |
| Operator-grade audit log | yes (hash-chained on-disk log) | no | no | yes | yes |
| Minimal scope (no scheduler or retention engine) | yes | yes | yes | no | no |
| OS-portable architecture | yes (seams kept; no Windows port yet) | partial | partial | partial | no |
| Open-source licence | Apache-2.0 (code) | various | various | AGPL/GPL | no |

Remanence leads on most rows because the alternatives took on more responsibility
than the mechanism layer needs. A smaller scope means fewer failure modes.

---

## vs raw SCSI utilities (`mt`, `mtx`, `sg_*`)

Most experienced operators build a personal collection of `mt` and `mtx` scripts.
They work, and they are the most direct way to talk to tape. Remanence does not
replace these tools for ad-hoc debugging. It replaces them in systems that must
talk to tape reliably, day after day, without an operator at the keyboard.

Eight things Remanence does that an `mt`/`mtx` script cannot:

### 1. Device naming is stable

`mtx` references libraries via `/dev/sg7` (or similar); `mt` references drives via
`/dev/nst2`. Both are kernel-enumeration paths that renumber silently after a
reboot, a rescan of the host bus adapter (HBA), a driver reload, or a drive replacement. A script that
worked yesterday can fail today by talking to the wrong drive.

Remanence derives every operation by drive serial number, read from VPD (Vital Product Data) page 0x80,
which holds the unit serial number, of the device itself. `/dev/sg7` and `/dev/nst2` are treated as temporary labels:
they are resolved afresh on every discovery, and the persistent state never refers
to them.

### 2. The library ↔ drive join is automatic

`mtx` knows the library's view: "drive bay 2 contains tape `S30002L9`, currently at
element address 0x0102." `mt` knows the host's view: `/dev/nst2` is a tape device
with these capabilities. Nothing joins the two. Operators either maintain the
mapping by hand or rely on conventions that break under §1 above.

Remanence's discovery layer issues `READ ELEMENT STATUS` to the library (with the
DVCID and CurData bits set, which HPE firmware requires) and `INQUIRY` + VPD 0x80
to every kernel sg device, then joins by serial number. The resulting model says
"bay X of library L contains drive S, which is reachable at `/dev/sgN` right now",
and "right now" is re-derived on each discovery call.

### 3. Partitioned-chassis safety

Enterprise libraries such as the HPE MSL3040 support partitioning: one physical
chassis presents as several independent logical libraries. A typical case is an LTO-9 partition owned by Remanence and an LTO-7 partition owned
by a second archive system. They share the chassis but must never touch each other's
cartridges.

`mtx -f /dev/sg11 move s1 d1` run against the wrong partition by mistake silently
corrupts another system's archive. `mtx` has no safeguard against this.

Remanence uses a per-library allowlist: state-changing operations refuse to run
against any library not on the list. For the direct library commands the list is given
with `--allow` (the library's serial number, repeatable); `rem tape init` takes its
list from the configured libraries; `rem tape retire` changes only the catalog and
needs none. Read-only inspection works on every reachable library; state-changing operations
require an explicit opt-in.
This lets Remanence share a host with another tape system safely, without the
operator having to be careful with each command.

A standalone drive has the same fail-closed rule without inventing a library
identity. Discovery must prove that no changer claims it, and every direct
operation names both the current `/dev/sgN` path and the drive serial it is
allowed to find there. Incomplete ownership evidence permits only explicitly
acknowledged reads; it never authorizes a write.

### 4. Phase-aware composed operations

"Load slot 5 into drive 2" is two SCSI commands:
- `MOVE MEDIUM` on the changer (slot → drive bay).
- `LOAD` on the drive (engage the heads).

If the second fails, the first has already succeeded and the cartridge is sitting
in the bay, unloaded. `mt`+`mtx` give no help in untangling this; the operator or
the script has to remember which state to recover from.

Remanence's composed `load` operation returns a phase-aware error type.
`LoadError::Move` means the changer rejected the MOVE (cartridge still in the
slot); `LoadError::OpenDrive` means the MOVE succeeded but the drive could not be
opened; `LoadError::DriveLoad` means the drive rejected the SCSI LOAD. Each variant
documents the resulting physical state and which recovery action makes sense. The
CLI prints the matching hint; daemon callers branch on the variant.

### 5. Completion-unknown handling

`mtx` returns "OK" or "error". After a transport-level failure (SCSI timeout,
kernel I/O error, cable disturbance) you do not know whether the cartridge moved:
the drive may have completed the command and the status reply may have been lost.

For tape I/O, Remanence distinguishes three kinds of failure (robotics commands use a
simpler classification of their own):
- `CHECK CONDITION` for a known refusal: the device actively refused the command.
  The physical state is known and the snapshot stays clean. Two kinds of sense data
  are treated differently. A deferred error (reported against an earlier command) is
  classified as completion-unknown. A reset UNIT ATTENTION invalidates the drive's head
  position and block-size configuration, which Remanence re-establishes before any
  further read or write.
- Transport error or timeout (`CompletionUnknown`): the outcome is not known. The
  snapshot is marked dirty with cause `CompletionUnknown`. The CLI prints "refresh
  or rescan before acting on either endpoint" and does not report a state it
  cannot confirm.
- Partial failure: a multi-step operation where one step succeeded and a later step
  failed in a known way. The snapshot is patched to reflect the partial state and
  marked dirty with cause `PartialFailure`.

The worst outcome for an irreplaceable tape is a state that looks fine and is not.
The dirty-state machine exists to prevent that outcome.

### 6. Per-operation-class SCSI timeouts

The kernel's default `SG_IO` timeout is short (5 seconds on most distributions).
Real LTO operations are not short:
- `MOVE MEDIUM` on a busy chassis: 8–20 s.
- `LOAD` on a cold LTO-9 cartridge: up to two hours (first-load calibration).
- `INITIALIZE ELEMENT STATUS` on a 280-slot chassis: minutes.

`mt`+`mtx` use the default timeout for everything, so real-hardware operations fail
and look like cable problems.

Remanence assigns a timeout class to each CDB: `Inquiry = 5s`, `Move = 120s`,
`LoadUnload = 600s`, `InitElementStatus = 600s`, `ReadElementStatus = 60s`. The
class is set on the transport before each command is dispatched. Operations get the
time they need, and a timeout that does occur is classified as `CompletionUnknown`
(§5).

### 7. Hot-plug awareness

`mt`+`mtx` are command-line snapshots. They do not notice that a cable was reseated
or a drive was replaced, and the operator has to remember to re-run discovery.

Remanence's Layer 2c watcher subscribes to udev events on the SCSI subsystems and
coalesces bursts on a 500 ms sliding window (a cable reseat fires dozens of events
in 300 ms, and the watcher collapses them into one). It then tells the daemon that
something changed so the daemon can re-derive its state. Without the watcher, an
operator pulling a drive while the daemon is running leaves the daemon's in-memory
state out of step with reality.

### 8. Auditability and testability

`mt`+`mtx` produce no structured record of what they did. The audit trail is
whatever you `tee` into a logfile, and their internals cannot be reached by
automated tests.

Remanence emits an audit event for every state-changing operation: `Started` (with
the CDB bytes), `Finished` (with the outcome: Success, ScsiError or Other), and
`Warning` (for example, reconciliation observations after a refresh). The events
are persisted in a hash-chained audit log on disk. Per-tape journals record durable
tape-file commits, and the SQLite catalog is a rebuildable projection rather than
the only copy of the truth. Automated tests run against captured SCSI response
fixtures and virtual media. The fixtures cover the HPE DVCID firmware requirement,
timeout classes, sense-data classification, and the completion-unknown and
partial-failure paths, so these behaviours are tested without a physical chassis.

### When `mt`+`mtx` is still the right tool

For interactive debugging. If you have logged into the host because something is
misbehaving and you want to inspect the hardware directly, `mtx -f /dev/sg7 status`
and `mt -f /dev/nst2 status` are still the right commands. They are
single-purpose, well documented, and finish in seconds.

Remanence replaces them for the system path, where a long-running service does
archive work day after day without an operator at the keyboard. The two coexist.

---

## vs LTFS (any implementation)

LTFS is attractive at first reading: mount the tape, `cp file /mnt`, unmount. In
practice there are three problems.

1. It models tape as random-access. Tape is sequential, and the access patterns
   LTFS encourages (`ls`, `cp -R`, random reads against many files) cause
   back-and-forth repositioning of the tape ("shoeshining") and wear it faster. The
   operating-system tooling above LTFS does not know about tape's costs.

2. State corruption from an improper unmount is a real operational problem. Pulling
   power, killing the LTFS process, or hitting a kernel oops during an open mount
   can leave a tape whose LTFS metadata block is inconsistent with the data on
   tape. Recovery requires vendor-specific tools that are not always part of the
   distribution. We have seen this in production.

3. Cross-vendor interoperability is weaker than commonly claimed. Encrypted LTFS
   volumes written by one vendor's tooling often do not read on another vendor's,
   and near-full tapes show similar variance. For an archive that has to stay
   readable for 10 to 20 years, across several generations of drives and software,
   this is a real risk.

Remanence takes a different approach, documented in
[REM-OBJECT Core Format 1.0](../specs/publication/rem-object-core-1-specification.md).
There is one object format, `rem-object-v1`: a chunk-aligned, constrained POSIX pax
tar stream with a deterministic CBOR manifest. A plaintext REM-OBJECT can be
extracted with a standard pax-aware `tar` implementation even if Remanence itself is
unavailable. An encrypted copy must first be opened according to
[REM-ENCRYPT 1.0](../specs/publication/rem-encrypt-1-specification.md); the result
is the same canonical pax tar stream. On tape,
[REM-PARITY 1.0](../specs/publication/rem-parity-1-specification.md) adds
Reed-Solomon sidecar parity and catalog-less bootstrap recovery. Version 0.1.0 reads
and writes this stack end to end, and the physical path has been validated on an HPE
MSL3040 library with LTO-9 drives.

If your workflow is interactive `cp` against a tape, LTFS is the right choice. If
your workflow is to write irreplaceable data once and retrieve it on operator
request years later, LTFS is a poor fit, and that second workflow is the one
Remanence serves.

---

## vs full backup suites (Bacula, Bareos, NetBackup)

These are capable systems, open-source or commercial, and for many use cases they
are the right choice. The argument against them for an archive is that they do too
much, and the extra parts are not optional.

A backup suite ships with:
- Its own catalog database (MySQL/PostgreSQL/MariaDB).
- Its own scheduler ("run backup of client X every night at 2 am").
- Its own retention policy engine ("keep daily for 30 days, weekly for 12 months,
  ...").
- Its own concept of a "job" with state across many tapes.
- Its own on-tape format, usually proprietary or quasi-proprietary.
- Its own access control and operator UI.

If your needs match what those modules do, this is fine. If they do not, you have
to work around every module: bend the scheduler to behave the way your orchestrator
wants, and replicate retention policy across the suite's settings and your real
source-of-truth catalog.

There are three further costs.

- The on-tape format belongs to the suite. Recovering data after the vendor stops
  shipping is hard. Bareos and Bacula remain open source, which reduces the risk;
  for commercial suites it is a serious risk to a 10-to-20-year archive.
- The catalog database is a single point of failure. If you lose the Bacula catalog,
  you lose the index of what is on every tape. The tape contents are still
  recoverable in principle (each tape has its own label blocks), but the operational
  story is rough.
- Migration is hard. Once a petabyte sits in Bareos's format, moving to anything else
  is a multi-month project.

Remanence owns only the tape mechanism. Retention, scheduling, and workflow are the
orchestrator's job. Its SQLite catalog answers the mechanism-level questions (which
tape holds an object, where its files begin, and what was committed), but it is a
rebuildable projection and not the archive's sole source of truth. Every tape
describes itself through REM-OBJECT manifests and REM-PARITY bootstraps; per-tape
journals and the hash-chained audit log record the host-side operational history.

Version 0.1.0 implements this design across all five layers. The pipelined tape I/O
path uses a staging ring and device position proofs; the daemon exposes catalog,
read, write, robotics, and stewardship operations through gRPC over a Unix socket or
optional mTLS TCP. The implementation reads and writes physical tape end to end.

You can stack Remanence under a backup suite as a substrate. We do not expect anyone
to do so, because the suites prefer to drive the SCSI themselves, but the
architecture allows it.

---

## vs proprietary archive systems (Atempo Miria, IBM Spectrum, Veritas NetBackup, ...)

The vendors' offer is to take care of everything for a fee, and for some shops that
is the right arrangement. For an archive that has to stay readable for 10 to 20
years, three structural problems appear.

1. Format lock-in. The on-tape format is proprietary. Over 10 to 20 years the vendor
   may stop supporting the version that wrote your tapes, and the product may stop
   running on the operating systems you can buy.

2. Opaque failure modes. When something goes wrong, the audit trail is whatever the
   vendor exposes. Often that is a support ticket: "please ship us your log files;
   we'll get back to you in 3 business days." An archival operator trying to recover
   a tape now needs more than that.

3. License cost compounds. Per-TB licensing across years of growth adds up to more
   than the storage media itself. The cost model does not fit data that is written
   once and held for a long time.

Remanence's reference implementation is Apache-2.0, its specifications are
CC-BY-4.0, and its conformance vectors are CC0-1.0. REM-OBJECT Core Format 1.0,
REM-ENCRYPT 1.0, and REM-PARITY 1.0 are published, implemented end to end, and
pinned by test vectors, as review drafts (see
[Status and maturity](#status-and-maturity)). If the project disappears, a plaintext
REM-OBJECT remains a constrained pax tar archive readable by standard tools. An
encrypted object requires a matching key and an implementation of the published
REM-ENCRYPT profile before the same tar stream can be extracted. REM-PARITY
documents how to recover object bytes from the tape's Reed-Solomon sidecars and
bootstrap records without a catalog.

The proprietary vendors have real features that Remanence does not: full archive
workflows, polished UIs, paid support, certified configurations. For the specific
archive shape Remanence targets, the trade-off they ask for is a poor fit.

---

## What Remanence does not do

These non-goals are listed so you can rule Remanence out quickly if it does not fit.

- No archive workflow. Remanence does not decide what to archive, when, where to put
  the copies, when to retire them, or how to notify users. That is the
  orchestrator's job.
- No cross-tier coordination. Disk caches, cloud tiers, and hot storage migration
  are out of scope.
- No deduplication. Stream-level or block-level dedup is a pre-tape concern.
- No user authentication. Authentication at the API layer (mTLS) identifies
  machines. End-user authentication is the orchestrator's job.
- No operator UI. A `rem` CLI exists for debugging and operator emergency
  intervention. There is no daemon UI.
- No tape lifecycle automation beyond the daemon's own stewardship. Migration to a
  new tape generation and tape retirement on a bit-error-rate threshold can be built
  against Remanence's API, but they are not in the daemon. The daemon does clean drives
  automatically: for managed drives it runs a cleaning sequence using cleaning
  cartridges recognized by barcode prefix (default `CLN`), subject to a minimum interval
  between cleanings and a weekly cap. A drive that is cleaned too often is flagged with
  an alarm. Cleaning is configured under `[cleaning]` in
  [reference-configuration.md](reference-configuration.md).
- No replication across libraries or hosts. A single Remanence daemon manages every
  library on its host; multi-host coordination is the orchestrator's.
- No ordinary Object ingest onto WORM media. Whole-Object crash recovery replaces an
  uncommitted tail, so write sessions require the drive to positively report
  rewritable media. Read-only recovery and the append-only terminal-index
  finalization path remain WORM-aware.

If any of these is a requirement for you, Remanence is the wrong tool.

---

## When you want Remanence

You want Remanence if:

- You are building (or already have) an orchestrator-shaped archive system, and you
  want the tape layer to be small, open, auditable, and testable.
- You operate several tape generations side by side (LTO-7 alongside LTO-9 on the
  same host) and you need partition-level safety properties.
- You write data to tape that must stay readable for 10 to 20 years as drives,
  software, and staff change, ideally with no software dependency on Remanence
  itself.
- You want the failure surface to be explicit and legible to the operator, and not
  hidden behind a vendor's support portal.
- You are willing to pair Remanence with an orchestrator that does the workflow side.
  You can write the orchestrator, and Remanence's API is designed to be easy to
  integrate with.

You probably want something else if:

- You want a turnkey backup product. Bareos is the open-source default; the
  commercial vendors all do this well.
- You want LTFS-style "mount the tape and use it like a disk." LTFS itself is the
  right choice there.
- You have one tape drive, no library, and you want to write a directory to it.
  `tar -cf /dev/nst0 mydir` does that, and you do not need a daemon.

---

## Status and maturity

Remanence is alpha software at version 0.1.0. All five layers are implemented,
including end-to-end tape reads and writes. It has been validated on one library
family and one drive generation, so restore and verify real material on your own
hardware before relying on it. The implementation has not had the operational
testing that a 1.0 release implies.

The on-tape formats are REM-OBJECT Core Format 1.0 (wire id `rem-object-v1`),
REM-ENCRYPT 1.0, and REM-PARITY 1.0. Each is specified, implemented, and
accompanied by pinned test vectors. The software version and the format versions are
independent: the format versions stay at 1.0 when the software is at 0.1.0. The
specifications are review drafts, not frozen. They freeze on 31 July 2027, and the
durability guarantees described in this document take effect then. For the current
capability and limitations, see the [README](../README.md) and [status.md](status.md). If this
document and a specification differ, follow the specification.

---

## Pointers

- [REM-OBJECT Core Format 1.0](../specs/publication/rem-object-core-1-specification.md)
  — the canonical object container, manifest, identity, and range-read rules.
- [REM-ENCRYPT 1.0](../specs/publication/rem-encrypt-1-specification.md) — the
  encrypted representation and its X-Wing hybrid KEM profile (X25519 +
  ML-KEM-768).
- [REM-PARITY 1.0](../specs/publication/rem-parity-1-specification.md) —
  the Reed-Solomon tape layout, sidecars, and catalog-less bootstrap recovery.
- [Architecture overview](architecture-overview.md) — the layer stack,
  write/read paths, catalog, audit log, journals, and daemon surface.
- `pfr-reference.md` — partial-file-restore mechanics, including the
  prior-art comparison with LTFS extents.
- `INSTALL.md` — operator runbook for the dev/reference deployment.
- [README](../README.md) — current status and quick start.
