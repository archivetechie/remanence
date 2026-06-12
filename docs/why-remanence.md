# Why Remanence?

A short positioning document. If you are choosing between Remanence and
another way of putting data on LTO tape, this explains the trade-offs
in operator-grade terms — not a marketing pitch, not a spec. The
formal commitments live in `spec-v0.3.md`; this doc explains the
*reasoning* behind those commitments by comparing Remanence to the
alternatives.

---

## What Remanence is, in one sentence

**Remanence is the tape-mechanics layer for LTO archives — a faithful
mechanism for reading, writing, locating, and accounting for data on
tape, with the safety properties an archival operator actually needs.**

The mental model is a filesystem driver: it does not decide *what* to
archive, *when* to archive, or *for how long* to keep things. It does
the mechanism of putting bytes on physical media in a way that's
durable, recoverable, and operationally legible. Higher-level archive
workflows — retention policy, cross-tier orchestration, deduplication,
job scheduling, user-facing UI — are explicitly out of scope, and
belong to the orchestrator that calls Remanence's API.

This is a deliberate choice, and it's the central thing that makes
Remanence different from every alternative in the space. The
alternatives generally fall into four categories, each with a real
weakness for the archival use case.

---

## The four categories of alternative

| Category | Examples | Strength | Weakness for long-term archives |
|--|--|--|--|
| **Raw SCSI utilities** | `mt`, `mtx`, `sg_*` from sg3-utils | Universal, scriptable, no daemon | No state model, no safety properties, no compositional reasoning |
| **Tape-as-filesystem** | LTFS (any implementation) | "Mount the tape and `cp`" | Pretends tape is random-access; fragile under interrupted writes; inter-vendor compatibility is weaker than claimed |
| **Full backup suites** | Bacula, Bareos, BackupExec | Complete workflow out of the box | Does *everything*, so you inherit *everything*: their catalog DB, their retention policy, their scheduler, their concept of "job" |
| **Proprietary archive systems** | Atempo Miria, IBM Spectrum, Veritas NetBackup | Vendor-supported "complete solution" | Black-box format, vendor lock-in, opaque failure modes, license cost compounds over decades |

Remanence is none of these. It is deliberately smaller than every
category above — and that's the value.

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
| Hot-plug aware (re-derive on udev events) | yes | no | n/a | depends | yes |
| Unit-testable without hardware | yes (~180 tests) | no | no | partial | no |
| Data readable in 30 years with standard tools | yes (pax-tar option, specified) † | n/a (raw bytes) | only if LTFS persists | suite-specific | proprietary |
| Operator-grade audit log | yes (event hook today; hash-chained log specified) † | no | no | yes | yes |
| Minimal scope (no scheduler, no retention, no catalog) | yes | yes | yes | no | no |
| OS-portable architecture (seams for Windows port) | yes | partial | partial | partial | no |
| Open-source licence | AGPL-3.0-or-later | various | various | AGPL/GPL | no |

**†** Rows marked with † are properties Remanence is **specified
to have** in the layered design but that are not yet implemented
in v0.0.1. The audit-event *hook* is live in Layer 2 today;
the hash-chained on-disk log is Layer 4, specified but not built.
The on-tape format trait + built-ins (`pax-tar-v1`,
`rem-chunked-v1`) are Layer 3b, specified but not built. See
`spec-v0.3.md` §10 for per-milestone status. v0.0.1's
demonstrated capability stops at Layer 2 (discovery + state
changes + hot-plug); the comparison table reflects the
architectural commitment, not v0.0.1 binaries.

The Remanence column wins on most rows precisely because *the
alternatives took on more responsibility than the mechanism layer
needs to*. Less software does less wrong.

---

## vs raw SCSI utilities (`mt`, `mtx`, `sg_*`)

Most experienced operators eventually build a personal collection of
`mt` and `mtx` scripts. They work. They are the bare-minimum way to
talk to tape. **Remanence does not replace these tools for ad-hoc
debugging.** It replaces them for the systems that need to talk to
tape *reliably*, day in and day out, without an operator at the
keyboard.

Eight concrete things Remanence does that a `mt`/`mtx` script
fundamentally cannot:

### 1. Device naming is stable

`mtx` references libraries via `/dev/sg7` (or similar); `mt`
references drives via `/dev/nst2`. Both are kernel-enumeration paths
that **renumber silently** after a reboot, an HBA rescan, a driver
reload, or a drive replacement. A script that worked yesterday can
fail today by talking to the wrong drive.

Remanence derives every operation by **drive serial number** (read
from VPD page 0x80 of the device itself). `/dev/sg7` and `/dev/nst2`
are treated as ephemeral labels — they get resolved fresh on every
discovery, and the persistent state never refers to them.

### 2. The library ↔ drive join is automatic

`mtx` knows the library's view: "drive bay 2 contains tape
`S30002L9`, currently at element address 0x0102." `mt` knows the
host's view: `/dev/nst2` is a tape device with these capabilities.
**Nothing joins them.** Operators either maintain the mapping by
hand or rely on conventions that break under §1 above.

Remanence's discovery layer issues `READ ELEMENT STATUS` to the
library (with the DVCID + CurData bits set — a quirk we found the
hard way on HPE firmware) and `INQUIRY` + VPD 0x80 to every kernel
sg device, then joins by serial number. The resulting model says
"bay X of library L contains drive S, which is reachable at
`/dev/sgN` *right now*" — and "right now" is re-derived on each
discovery call.

### 3. Partitioned-chassis safety

Enterprise libraries like the HPE MSL3040 support **partitioning**:
one physical chassis presents as several independent logical
libraries. In the typical archive deployment the LTO-9 partition is
owned by Remanence and the LTO-7 partition by `dwara2`. They share
the chassis but must never touch each other's cartridges.

`mtx -f /dev/sg11 move s1 d1` against the wrong partition by mistake
silently corrupts another system's archive. There is no safeguard.

Remanence takes a **per-library allowlist**: state-changing
operations refuse to run against any library not on `--allow`.
Read-only inspection works on every reachable library (visibility
is fine; action requires explicit opt-in). This lets Remanence
share a host with another tape system safely — without the
operator having to be careful with each command.

### 4. Phase-aware composed operations

"Load slot 5 into drive 2" is two SCSI commands:
- `MOVE MEDIUM` on the changer (slot → drive bay).
- `LOAD` on the drive (engage the heads).

If the second fails, the first already succeeded — the cartridge
is now sitting in the bay, unloaded. `mt`+`mtx` give you no help
in untangling this; the operator (or the script) has to remember
what state to recover from.

Remanence's composed `load` operation returns a phase-aware error
type. `LoadError::Move` tells you the changer rejected the MOVE
(cartridge still in the slot); `LoadError::OpenDrive` tells you
the MOVE succeeded but the drive couldn't be opened; `LoadError::
DriveLoad` tells you the drive rejected the SCSI LOAD specifically.
Each variant documents the resulting physical state and which
recovery action makes sense. The CLI prints the matching hint;
daemon callers branch on the variant.

### 5. Completion-unknown handling

`mtx` returns "OK" or "error." On a **transport-level** failure
(SCSI timeout, kernel I/O error, cable disturbance) you don't know
whether the cartridge actually moved or not — the drive may have
completed the command and we just lost the status reply.

Remanence distinguishes three flavours of failure for every
state-changing operation:
- **`CHECK CONDITION`** — the device actively refused the
  command. Physical state is known. Snapshot stays clean.
- **Transport error / timeout (CompletionUnknown)** — we don't
  know what happened. Snapshot is marked **dirty** with cause
  `CompletionUnknown`. The CLI prints "refresh or rescan before
  acting on either endpoint" — and refuses to silently lie about
  the state.
- **Partial failure** — multi-step op where one step succeeded
  and a later step failed in a known way. Snapshot patched to
  reflect the partial state, marked dirty with cause
  `PartialFailure`.

This is the safety property archivists actually care about. The
worst outcome for an irreplaceable tape is "looks fine, isn't"
— the dirty-state machine refuses to produce that outcome.

### 6. Per-operation-class SCSI timeouts

The kernel's default `SG_IO` timeout is short (5 seconds on most
distributions). Real LTO operations are not short:
- `MOVE MEDIUM` on a busy chassis: 8–20 s.
- `LOAD` on a cold LTO-9 cartridge: up to two hours (first-load
  calibration).
- `INITIALIZE ELEMENT STATUS` on a 280-slot chassis: minutes.

`mt`+`mtx` use the default timeout for everything. Real-hardware
operations silently fail and look like cable problems.

Remanence assigns a timeout *class* per CDB. `Inquiry = 5s`,
`Move = 120s`, `LoadUnload = 600s`, `InitElementStatus = 600s`,
`ReadElementStatus = 60s`. The class is set on the transport
before each command is dispatched. Operations get the time they
actually need; spurious timeouts get classified as
`CompletionUnknown` per §5 above.

### 7. Hot-plug awareness

`mt`+`mtx` are command-line snapshots. They never notice that a
cable was reseated or a drive was replaced. The operator has to
remember to re-run discovery.

Remanence's Layer 2c watcher subscribes to udev events on the
SCSI subsystems, coalesces bursts on a 500 ms sliding window (a
cable reseat fires dozens of events in 300 ms; we collapse them
into one), and tells the daemon "something changed" so it can
re-derive. Without this, an operator pulling a drive while the
daemon is running silently desynchronises its in-memory state
from reality.

### 8. Auditability and testability

`mt`+`mtx` produce no structured record of what they did. The
audit trail is whatever you `tee` into a logfile. Their internals
are unreachable for automated testing; you ship hope.

Remanence emits an **audit event** for every state-changing
operation: `Started` (with the CDB bytes), `Finished` (with the
outcome — Success / ScsiError / Other), `Warning` (e.g.
reconciliation observations after a refresh). The hook is live in
Layer 2 today; the persistent **hash-chained audit log on disk** is
specified in spec §3.3 / §8.2 and lands in Layer 4 (not yet
built). The whole stack has ~180 unit tests running against
captured SCSI response fixtures, so we caught the HPE DVCID
firmware quirk, the timeout-too-short class of bugs, and several
others before touching the production chassis.

### Summary: when `mt`+`mtx` is still the right tool

Honest answer: for **interactive debugging**. If you've SSH'd
into the host because something is misbehaving and you want to
poke the hardware directly, the right reach is still
`mtx -f /dev/sg7 status` and `mt -f /dev/nst2 status`. They are
single-purpose, well-documented, and operate in seconds.

Remanence replaces them for the **system path**: the path where
a long-running service does archive work day after day without
an operator at the keyboard. The two coexist; they're not
either-or.

---

## vs LTFS (any implementation)

LTFS is appealing on first read. *"Mount the tape; `cp file /mnt;
umount."* What's not to love?

In practice, three things:

1. **It models tape as random-access.** Tape is sequential, and
   the access patterns LTFS encourages — `ls`, `cp -R`, random
   reads against many files — produce shoeshining and wear out
   tape faster. The operating-system tooling above LTFS does not
   know about tape's costs.

2. **State corruption from improper unmount is a real
   operational problem.** Pulling power, killing the LTFS
   process, hitting a kernel oops during an open mount — any of
   these can leave a tape in a state where the LTFS metadata
   block is inconsistent with the data on tape. Recovery
   requires vendor-specific tools that aren't always part of the
   distribution. We've seen this in production at archive.

3. **Cross-vendor interoperability is weaker than commonly
   claimed.** Encrypted LTFS volumes written by one vendor's
   tooling often do not read on another vendor's; near-full tapes
   exhibit similar variance. For a 30-year archive horizon this
   is a real risk.

Remanence's on-tape format (`spec-v0.3.md` §5) explicitly rejects
LTFS for these reasons. The architectural commitment (specified
in spec v0.3 §5, **not yet implemented** as of v0.0.1) is:
the default body format `rem-chunked-v1` will be a chunked
container with a documented binary schema, and an alternative
format `pax-tar-v1` will be offered for maximum portability —
once Layer 3b ships, a tape written in pax tar will be readable
in 2070 with any Unix-like system's `tar` command, no specialised
software required. v0.0.1 does not yet write tape data of any
kind; spec §10 milestones M3a / M3b track the work.

If your workflow is "interactive `cp` against a tape," LTFS is
the right answer today and probably remains the right answer.
If your workflow is "write irreplaceable data once, retrieve it
on operator request a decade later," LTFS isn't — and Remanence
will be once Layer 3b lands.

---

## vs full backup suites (Bacula, Bareos, NetBackup)

These are real, capable, open-source (or commercial) systems that
do work. We're not dismissing them — for many use cases they are
the right call. The argument against them in *our* use case
reduces to: **they do too much, and the extras are not optional.**

A backup suite ships with:
- Its own catalog database (MySQL/PostgreSQL/MariaDB).
- Its own scheduler ("run backup of client X every night at 2 am").
- Its own retention policy engine ("keep daily for 30 days, weekly
  for 12 months, ...").
- Its own concept of a "job" with state across many tapes.
- Its own on-tape format, usually proprietary or quasi-proprietary.
- Its own access control and operator UI.

If your needs match what those modules do, this is fine. If they
don't, you get to fight every module: bend the scheduler to behave
the way your orchestrator wants, replicate retention policy across
the suite's settings and your real source-of-truth catalog, etc.

The other costs:

- **The on-tape format is theirs.** Recovering data after the
  vendor stops shipping is hard. Bareos and Bacula remain open
  source, so this is mitigated; for commercial suites it's a
  serious archive-horizon risk.
- **The catalog database is a single point of failure.** Lose the
  Bacula catalog, lose the index of what's on every tape. The
  tape contents are still recoverable in principle (each tape
  has its own label blocks), but the operational story is rough.
- **Migration is hard.** Once a petabyte sits in Bareos's format,
  moving to anything else is a multi-month project.

Remanence's architectural commitment (per spec v0.3): own only
the tape mechanism. Catalog, retention, scheduling, and workflow
are all the orchestrator's job. The on-tape format is open and
documented (so a future reader does not need Remanence itself to
interpret the bytes; see `spec-v0.3.md` §5.8). There is no
separate database — every tape will self-describe
(`spec-v0.3.md` §3.3); the disk-side state will be a regenerable
cache plus an append-only audit log, nothing that needs
migration.

As of v0.0.1 the audit-event hook is live in Layer 2 today; the
on-tape format, the disk-side cache, and the hash-chained
on-disk audit log are specified in spec §5 / §3.3 / §8.2 but not
yet built (M3b + M4 in spec §10). The comparison in this section
reflects the v0.3 commitment; v0.0.1 binaries can be evaluated
against the live Layer 2 surface only.

You can stack Remanence under a backup suite if you wanted to,
treating it as a substrate. We don't expect anyone to actually
do that — the suites prefer to drive the SCSI themselves — but
the architecture admits it.

---

## vs proprietary archive systems (Atempo Miria, IBM Spectrum, Veritas NetBackup, ...)

The honest pitch from these vendors is "we'll take care of
everything; you pay us." For some shops that's the right deal.
For an archive with a 30-year horizon, three structural problems
appear:

1. **Format lock-in.** The on-tape format is proprietary. After
   30 years there is no guarantee the vendor will still exist,
   still support the version that wrote your tapes, or still
   sell software that runs on the operating system you can buy.

2. **Opaque failure modes.** When something goes wrong, the
   audit trail is whatever the vendor exposes. Often that's a
   support ticket: "please ship us your log files; we'll get
   back to you in 3 business days." For an archival operator
   trying to recover a tape *now*, this is the wrong shape.

3. **License cost compounds.** Per-TB licensing across decades
   of growth adds up to a number larger than the storage media
   itself. The cost model is fundamentally misaligned with
   "write once, hold for decades."

Remanence is AGPL-3.0-or-later. The source is the spec; the
on-tape format is **specified** (`spec-v0.3.md` §5) and will be
documented end-to-end when Layer 3b lands; the audit log will be
text on a disk that the operator owns (Layer 4 — also still to
land). If the project disappears tomorrow after Layer 3b ships,
every tape it ever wrote will still be readable with `tar` (for
the pax-tar format) or with the published CBOR schema (for the
chunked format). This is the architectural commitment v0.3 of the
spec carries; v0.0.1 of the binaries is still pre-data-plane.

This is not a knock on the proprietary vendors — they have real
features Remanence does not (full archive workflows, polished
UIs, paid support, certified configurations). It's a statement
that for the specific archive shape Remanence targets, the
proprietary trade-off is the wrong one.

---

## What Remanence does NOT do

Honest non-goals, so you can rule it out fast if it doesn't fit:

- **No archive workflow.** Remanence does not decide what to
  archive, when, where to put the copies, when to retire, or how
  to notify users. That's the orchestrator's job (Dwara v3 in the
  archive deployment).
- **No cross-tier coordination.** Disk caches, cloud tiers, hot
  storage migration — out of scope.
- **No deduplication.** Stream-level or block-level dedup is a
  pre-tape concern.
- **No user authentication.** Authn at the API layer (mTLS) is
  for *machine* identity. End-user authentication is the
  orchestrator's job.
- **No operator UI.** A `rem` CLI exists for debugging and
  operator emergency intervention; a daemon UI is potentially a
  follow-on but not in scope today.
- **No tape lifecycle automation.** Migration to a new
  generation, tape retirement on bit-error-rate threshold, drive
  cleaning workflows — these are tooling layers that *can* be
  built against Remanence's API, but they aren't in the daemon.
- **No replication across libraries or hosts.** A single
  Remanence daemon manages every library on its host; multi-host
  coordination is the orchestrator's.
- **No write-once-read-many enforcement** beyond what the
  hardware itself does. We expose the SCSI primitives; policy
  decisions belong upstream.

If any of these are deal-breakers, Remanence is the wrong tool.

---

## When you want Remanence

You want Remanence if:

- You are building (or already have) an orchestrator-shaped
  archive system, and you want the tape layer to be small,
  open, audit-able, and testable.
- You operate on tape generations that need to coexist (LTO-7
  alongside LTO-9 on the same host) and you need partition-level
  safety properties.
- You write data to tape that must be readable in 30 years,
  ideally with no software dependency on Remanence itself.
- You want the failure surface to be explicit and operator-
  legible, not hidden behind a vendor's support portal.
- You are willing to pair Remanence with an orchestrator that
  does the workflow side. (You can write the orchestrator.
  Remanence's API is designed to be pleasant to integrate with.)

You probably want something else if:

- You want a turnkey backup product. Bareos is the open-source
  default; the commercial vendors all do this well.
- You want LTFS-style "mount the tape and use it like a disk."
  LTFS itself is the right answer there.
- You have one tape drive, no library, and you just want to dump
  a directory to it. `tar -cf /dev/nst0 mydir` is the right
  answer; you don't need a daemon.

---

## Pointers

- `spec-v0.3.md` — the formal specification. Architecture, scope
  boundary, security model, on-tape format, gRPC service surface.
- `layer2-design.md`, `layer2b-design.md`, `layer2c-design.md` —
  the design docs for the layers that are currently implemented.
- `pfr-reference.md` — partial-file-restore mechanics, including
  the prior-art comparison with LTFS extents.
- `INSTALL.md` — operator runbook for the dev/reference deployment.
- `README.md` — quick start.
