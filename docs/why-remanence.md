# Why Remanence?

<!-- code-anchor: crates/remanence-library/src/transport.rs crates/remanence-library/src/error.rs crates/remanence-library/src/discovery.rs crates/remanence-library/src/watch/mod.rs crates/remanence-state/src/config.rs crates/remanence-api/src/pool_write/model.rs @ 625f7044 -->

This page is for someone choosing between Remanence and another way of putting
data on LTO tape. It explains what Remanence is, compares it with the usual
alternatives, and says what it does not do. The format commitments themselves
are in the [REM-OBJECT Core Format 1.0](../specs/publication/rem-object-core-1-specification.md),
[REM-ENCRYPT 1.0](../specs/publication/rem-encrypt-1-specification.md) and
[REM-PARITY 1.0](../specs/publication/rem-parity-1-specification.md)
specifications; this page gives the reasoning behind them. The maturity of the
software is summarized under [Status and maturity](#status-and-maturity) and
set out in full in [What works, and what does not](status.md).

## What Remanence is, in one sentence

Remanence is the tape-mechanics layer for LTO archives: software that writes,
reads, locates and accounts for data on tape, and that refuses to act when it
cannot confirm the state of the hardware.

The closest analogy is a filesystem driver. Remanence does not decide what to
archive, when, or for how long. It puts bytes on tape, records where they are,
and reports what the hardware did. The archive workflow above it (retention
policy, coordination with disk or cloud tiers, deduplication, job scheduling,
any user interface) belongs to an orchestrator: the system that calls
Remanence's API and keeps its own records of what the archive is for.

The main difference between Remanence and the alternatives is this small scope.
The alternatives fall into four categories.

## The four categories of alternative

| Category | Examples | What it provides | Remarks for a long-term archive |
|--|--|--|--|
| Raw SCSI utilities | `mt`, `mtx`, and the `sg_*` tools from sg3_utils | Direct, scriptable control of drives and changers, with no daemon | Each command stands alone. A script has to track which cartridge is where, and recover from a command that half succeeded. |
| Tape as a filesystem | LTFS, in any implementation | A tape mounted as a filesystem, so ordinary file tools work on it | The tools above the filesystem do not know that tape is sequential, and reading a tape later depends on an LTFS implementation that reads its version. |
| Backup suites | Bacula, Bareos, Veritas NetBackup | A complete backup workflow: catalog database, scheduler, retention rules and a job model | The parts come together. A site that needs only the tape layer has to work around the rest. |
| Proprietary archive systems | Atempo Miria and similar vendor products | A supported product from one vendor that covers the whole archive | The on-tape format and the failure handling belong to the vendor, so reading the tapes later depends on the vendor's software. |

Remanence is smaller than any of these categories. It leaves workflow,
retention and scheduling to the orchestrator, and it publishes its on-tape
formats as specifications.

## Compared with raw SCSI utilities

Most experienced operators keep a collection of `mt` and `mtx` scripts. They
work, and they are the most direct way to talk to tape. Remanence does not
replace these tools for interactive debugging. It replaces them in systems that
must talk to tape reliably, day after day, without an operator at the keyboard.

The sections below describe eight things Remanence does on top of the raw
commands. A script could do each of them, but each would have to be written and
tested separately.

### 1. Device naming is stable

`mtx` addresses a library through a path such as `/dev/sg7`, and `mt` addresses
a drive through a path such as `/dev/nst2`. Both are kernel enumeration paths,
and they can change after a reboot, a rescan of the host bus adapter, a driver
reload or a drive replacement. A script that worked yesterday can then talk to
the wrong drive today.

Remanence identifies every device by its serial number, which it reads from the
device's vital product data (VPD) page 0x80, the page that holds the unit
serial number. It treats `/dev/sg7` and `/dev/nst2` as temporary labels: they
are resolved again on every discovery, and the persistent state never refers to
them.

### 2. The library and its drives are joined automatically

`mtx` knows the library's view: drive bay 2 holds tape `S30002L9`, at element
address 0x0102. `mt` knows the host's view: `/dev/nst2` is a tape device. Nothing
joins the two, so operators keep the mapping by hand or rely on conventions that
break for the reasons in section 1.

Remanence's discovery sends READ ELEMENT STATUS (the changer's inventory report)
to the library and INQUIRY plus the VPD serial page to every SCSI generic
device, then joins the two by serial number. The READ ELEMENT STATUS request
sets the DVCID bit, which asks the changer to include each drive's identity, and
the CurData bit, which asks it to answer from its current inventory without
moving anything. If the changer leaves a drive bay without a serial, discovery
retries with narrower requests. A bay that still cannot be mapped produces a
`DriveMappingUnavailable` warning, and state-changing operations on it are
refused. The result says "bay X of library L holds drive S, reachable at
`/dev/sgN` now", and "now" is derived again on each discovery.

### 3. Partitioned-chassis safety

Enterprise libraries such as the HPE MSL3040 can be partitioned: one physical
chassis presents itself as several independent logical libraries. A typical
case is an LTO-9 partition used by Remanence and an LTO-7 partition used by a
second archive system. They share the chassis but must never touch each other's
cartridges.

`mtx -f /dev/sg11 move s1 d1`, run against the wrong partition by mistake,
moves another system's cartridge without any warning. `mtx` has no record of
which partition belongs to whom.

Remanence keeps a per-library allowlist, and state-changing operations refuse to
run against a library that is not on it. The daemon and `rem tape init` take the
list from the libraries named in the configuration file. The `rem-debug`
commands that change library state take it from `--allow`, which names a
library serial and can be repeated. `rem tape retire` changes only the catalog
and needs no allowlist. Read-only inspection works on every library the host can
reach. As a result, Remanence can share a host with another tape system without
the operator having to take care over each command.

A standalone drive has the same rule without a library identity. Discovery must
prove that no changer claims the drive, and every direct operation names both
the current `/dev/sgN` path and the drive serial it is allowed to find there.
Incomplete ownership evidence permits only reads that the operator explicitly
acknowledges, and it never permits a write.

### 4. Composed operations report the phase that failed

"Load slot 5 into drive 2" is two SCSI commands: MOVE MEDIUM on the changer,
which carries the cartridge from the slot to the drive bay, and LOAD on the
drive, which threads the tape. If the second fails, the first has already
succeeded and the cartridge sits in the bay, unloaded. With `mt` and `mtx`, the
operator or the script has to work out which state it is in.

Remanence's composed `load` operation returns an error that names the phase
that failed. `LoadError::Move` means the changer rejected the move and the
cartridge is still in its slot. `LoadError::OpenDrive` means the move succeeded
but the drive could not be opened. `LoadError::DriveLoad` means the drive
rejected the LOAD. Each variant documents the physical state it leaves and the
recovery that makes sense, the CLI prints the matching hint, and daemon callers
branch on the variant.

### 5. Completion-unknown handling

`mtx` reports success or an error. After a transport-level failure (a SCSI
timeout, a kernel I/O error, a disturbed cable) nobody knows whether the
cartridge moved: the device may have completed the command and the status reply
may have been lost.

For tape I/O, Remanence distinguishes three kinds of failure. Robotics commands
use a simpler classification of their own.

- A CHECK CONDITION that reports a known refusal means the device refused the
  command. The physical state is known, and the library snapshot (the software's
  picture of the library) stays clean. A reset UNIT ATTENTION, which reports that
  the drive was reset, is treated specially: the drive's head position and block
  size are no longer known, and Remanence establishes them again before any
  further read or write.
- A transport error or a timeout means the outcome is unknown. The snapshot is
  marked dirty with the cause `CompletionUnknown`. The CLI warns that the device
  may have executed the command even though the host did not get a clean status
  back, tells the operator to confirm the physical state before retrying, and
  suggests `rem library <SERIAL> --slots` and `rem-debug rescan`.
- A partial failure is a multi-step operation in which one step succeeded and a
  later step failed in a known way. The snapshot is updated to the partial state
  and marked dirty with the cause `PartialFailure`.

A state that looks correct but is not is more dangerous than an error, because
nothing prompts anyone to check it. The dirty marker exists so that the software
never reports a position it has not confirmed.

### 6. Timeouts per command class

Tape operations take very different times. A changer move on a busy chassis
can take 8 to 20 seconds, and initializing the element status of a large
library can take minutes.

Remanence gives each SCSI command a timeout for its class: 5 s for INQUIRY and
other short status commands, 60 s for READ ELEMENT STATUS, 120 s for MOVE
MEDIUM, 300 s for positioning, 600 s for LOAD and UNLOAD, INITIALIZE ELEMENT
STATUS and REWIND, and 900 s for tape reads and writes and WRITE FILEMARKS. The
class is set on the transport before each command is sent. A command that does
time out is classified as completion-unknown (section 5).

The first load of a new LTO-9 cartridge is handled differently. It starts a
media optimization that can take up to two hours, during which the drive
answers NOT READY. Remanence does not stretch a timeout to cover it. The
daemon polls the drive with a readiness wait of up to 9000 s (2.5 hours) and
keeps robot moves waiting until the drive reports the medium ready.

### 7. Hot-plug events

`mt` and `mtx` run once and exit. They do not notice that a cable was reseated
or a drive replaced, and the operator has to remember to run discovery again.

`rem watch`, in a build with the optional `linux-udev` feature, subscribes to
udev events (the Linux device events) for the SCSI generic and SCSI tape
subsystems. It coalesces bursts within a 500 ms sliding window, so that a cable
reseat, which produces many events at once, is reported once. The watcher only
reports; it sends no SCSI commands and changes no state. The daemon does not
subscribe to these events.

### 8. Auditability and testability

`mt` and `mtx` leave no structured record of what they did. The audit trail is
whatever the operator saved from the terminal, and their internals cannot be
reached by automated tests.

Remanence records an audit event around each state-changing SCSI command:
`Started`, with the command bytes, before it is sent; `Finished`, with the
outcome, after it returns; `Refused`, when a safety check stops the operation;
and `Warning`, for non-fatal observations such as a mismatch found during a
refresh. The events go to a hash-chained audit log on disk. Each tape also has a
tape log, a host file that holds a durable commit record for every tape file
written. The SQLite catalog is a projection rebuilt from the tape logs and the
audit log, so it is never the only copy of anything.

Automated tests run against captured SCSI responses from real hardware and
against virtual media. The fixtures cover the identity reporting of real
changers, the timeout classes, sense-data classification, and the
completion-unknown and partial-failure paths, so these behaviours are tested
without a physical chassis.

### When `mt` and `mtx` are still the right tools

They remain the right tools for interactive debugging. If you have logged in
because something is misbehaving and you want to look at the hardware
directly, `mtx -f /dev/sg7 status` and `mt -f /dev/nst2 status` are still the
right commands. They do one thing, they are well documented, and they finish in
seconds.

Remanence replaces them on the system path, where a long-running service does
archive work without an operator at the keyboard. The two can be used on the
same host.

## Compared with LTFS

LTFS is attractive at first reading: mount the tape, `cp file /mnt`, unmount.
For an archive, two problems appear.

1. It presents tape as a random-access filesystem. Tape is sequential, and the
   access patterns that filesystem tools use (`ls`, `cp -R`, reading many files
   in no particular order) make the drive reposition the tape back and forth,
   which is slow and wears the tape. The tools above LTFS do not know what a
   seek costs.

2. An interrupted session is a real operational problem. Losing power, killing
   the LTFS process or a kernel failure while a tape is mounted can leave the
   tape's LTFS index out of step with the data on it. Repairing it depends on
   the check tool of the LTFS implementation that wrote it.

Reading an LTFS tape years later also depends on an LTFS implementation that
reads its format version. For an archive that has to stay readable for 10 to 20 years, across several
generations of drives and software, that dependency has to be planned for.

Remanence takes a different approach, set out in
[REM-OBJECT Core Format 1.0](../specs/publication/rem-object-core-1-specification.md).
There is one object format, `rem-object-v1`: a chunk-aligned, constrained POSIX
pax tar stream with a deterministic CBOR manifest. A plaintext object can be
extracted with a standard pax-aware `tar` even if Remanence is unavailable. An
encrypted copy must first be opened as
[REM-ENCRYPT 1.0](../specs/publication/rem-encrypt-1-specification.md)
describes, and the result is the same pax tar stream. On tape, REM-PARITY adds
Reed-Solomon sidecar parity and recovery from the bootstrap without a catalog.
Remanence reads and writes this stack end to end, and the physical path has
been tested on an HPE MSL3040 library with LTO-9 drives.

If your workflow is interactive `cp` against a tape, LTFS is the right choice.
If your workflow is to write irreplaceable data once and retrieve it on request
years later, LTFS is a poor fit, and that second workflow is the one Remanence
serves.

## Compared with backup suites

Bacula, Bareos and NetBackup are capable systems, open-source or commercial, and
for many uses they are the right choice. The argument against them for an
archive is that they do more than an archive's tape layer needs, and the extra
parts are not optional.

A backup suite comes with:

- its own catalog database (MySQL, PostgreSQL or MariaDB);
- its own scheduler ("back up client X every night at 2 am");
- its own retention rules ("keep daily backups for 30 days, weekly ones for 12
  months");
- its own idea of a job, with state across many tapes;
- its own on-tape format;
- its own access control and operator interface.

If your needs match what those parts do, this is fine. If they do not, you have
to work around each part: make the scheduler behave the way your orchestrator
wants, and keep the suite's retention settings in step with the catalog that is
your real source of truth.

There are three further costs. The main one is that the on-tape format belongs
to the suite, so reading the tapes after the product is gone is hard. Bareos and
Bacula are open source, which reduces this risk; for a commercial suite it is a
serious risk to a 10-to-20-year archive. A second cost is the catalog database:
if it is lost, the index of what is on every tape goes with it. The tapes still
carry the data and their own labels, so recovery is possible, but it means
scanning tapes to rebuild the index. A third is that once a large archive sits
in a suite's format, moving it to another system means reading every tape back.

Remanence owns only the tape mechanism. Retention, scheduling and workflow are
the orchestrator's job. Its catalog answers mechanism-level questions (which
tape holds an object, where its files begin, what was committed), but it is a
rebuildable projection and not the archive's only record. Every tape describes
itself through its object manifests and its bootstrap, and the tape logs and the
hash-chained audit log record the host-side history. The daemon serves catalog,
read, write, robotics and stewardship operations over gRPC, on a Unix socket or
optionally on TCP with mutual TLS (mTLS).

Remanence could be stacked under a backup suite as a substrate. We do not
expect anyone to do so, because the suites prefer to drive the SCSI themselves.

## Compared with proprietary archive systems

The vendors' offer is to take care of everything for a fee, and for some sites
that is the right arrangement. For an archive that has to stay readable for 10
to 20 years, three problems appear.

1. Format lock-in. The on-tape format is the vendor's. Over 10 to 20 years the
   vendor may stop supporting the version that wrote your tapes, and the
   product may stop running on the operating systems you can buy.

2. Opaque failure handling. When something goes wrong, the operator sees what
   the vendor exposes. Diagnosing a failure usually means sending logs to the
   vendor and waiting for an answer, which is slow when a tape needs recovering
   now.

3. License cost that grows with the archive. Licensing by capacity, over years
   of growth, can cost more than the media. That cost model fits data that
   changes, not data written once and kept for a long time.

Remanence's reference implementation is Apache-2.0, its specifications are
CC-BY-4.0, and its conformance vectors are CC0-1.0. The specifications are
review drafts (see [Status and maturity](#status-and-maturity)). REM-OBJECT
Core Format 1.0 and REM-ENCRYPT 1.0 are published, implemented end to end, and
pinned by test vectors. For REM-PARITY, the published copy describes an older
tape layout than the one current software writes; the text that matches current
software is in `specs/in-progress/` and has not been published yet. If the
project disappears, a plaintext object remains a constrained pax tar archive
that standard tools read. An encrypted object needs a matching key and an
implementation of REM-ENCRYPT before the same tar stream can be extracted.
REM-PARITY documents how to recover object bytes from the tape's Reed-Solomon
sidecars and bootstrap without a catalog.

The proprietary vendors have real features that Remanence does not: full archive
workflows, finished user interfaces, paid support and certified configurations.
For the kind of archive Remanence is built for, the trade-off they ask for is a
poor fit.

## What Remanence does not do

These non-goals are listed so that you can rule Remanence out quickly.

- No archive workflow. Remanence does not decide what to archive, when, where to
  put the copies, when to retire them, or whom to notify. That is the
  orchestrator's job.
- No coordination across storage tiers. Disk caches, cloud tiers and moving
  data between them are out of scope.
- No deduplication. Deduplication belongs before the data reaches tape.
- No user authentication. mTLS at the API identifies machines; authenticating
  people is the orchestrator's job.
- No graphical interface. `rem` is the operator command-line tool and `rem-debug`
  is the break-glass tool for direct hardware access. The daemon has no user
  interface of its own.
- Little tape lifecycle automation. Migrating to a new tape generation, or
  retiring tapes when their error rates rise, can be built on Remanence's API,
  but the daemon does not do it. The daemon does clean drives: for the drives it
  manages, it runs cleaning cartridges, recognized by barcode prefix (`CLN` by
  default), with a minimum interval between cleanings and a weekly cap, and it
  raises an alarm when a drive asks for cleaning too often. The `[cleaning]`
  section of the [configuration reference](reference-configuration.md) has the
  settings.
- No system backup. Remanence keeps file contents and `user.*` extended
  attributes, but not ownership, POSIX ACLs, security labels, file capabilities
  or file flags, so it cannot put a host back the way it was.
  [Why Remanence keeps only user extended attributes](why-user-xattrs-only.md)
  explains the decision and how to keep that metadata when you need it.
- No replication across hosts. A daemon manages the libraries named in its
  configuration on its own host. Coordination between hosts is the
  orchestrator's job.
- No ordinary object writes to WORM (write once, read many) media. Recovery from
  a crash during a write replaces the uncommitted tail of the tape, so writes
  require the drive to report the loaded cartridge as rewritable. Read-only
  recovery and the append-only finalization path handle WORM media.

If any of these is a requirement for you, Remanence is the wrong tool.

## When you want Remanence

You want Remanence if:

- you are building, or already have, an orchestrator, and you want a tape layer
  whose formats are published and whose behaviour can be tested without
  hardware;
- you run several tape generations side by side (LTO-7 beside LTO-9 on the same
  host, for example) and need the partition safety described above;
- you write data that must stay readable for 10 to 20 years as drives, software
  and staff change, ideally without depending on Remanence to read it;
- you want failures reported to the operator in terms of physical state, not
  hidden behind a support portal;
- you are willing to pair Remanence with an orchestrator for the workflow. You
  can write that orchestrator yourself against the gRPC API defined in
  [proto/layer5.proto](../proto/layer5.proto).

<!-- code-anchor: crates/remanence-cli/src/standalone.rs @ 625f7044 -->
One manually loaded tape drive also works. `rem setup`, `rem put` and `rem get`
operate it directly, without a daemon, and finalization writes the index that a
later restore without the catalog reads. Start with the
[standalone-drive guide](guide-standalone-drive.md).

You probably want something else if:

- you want a complete backup product. Bareos and Bacula are open-source options,
  and commercial suites do this too;
- you want to mount a tape and use it like a disk. LTFS is the right choice
  there.

## Status and maturity

Remanence is alpha software, and there is no binary release yet; you build it
from source. All five layers are implemented, including reading and writing
physical tape end to end. It has been tested on one library family and one
drive generation, so restore and verify real material on your own hardware
before relying on it. [What works, and what does not](status.md) gives the
version, the release status and the tested hardware.

The on-tape formats are REM-OBJECT Core Format 1.0 (wire identifier
`rem-object-v1`), REM-ENCRYPT 1.0 and REM-PARITY 1.0. The software version and
the format versions are independent: the formats stay at 1.0 while the software
is in its 0.x versions. The specifications are review drafts, not yet frozen.
They freeze on 31 July 2027, and the durability guarantees described on this
page take effect then. The REM-PARITY text that matches current software is
still in preparation, as [status](status.md#the-rem-parity-publication-copy)
explains. If this page and a specification differ, follow the specification.

## Pointers

- [REM-OBJECT Core Format 1.0](../specs/publication/rem-object-core-1-specification.md):
  the object container, manifest, identity and range-read rules.
- [REM-ENCRYPT 1.0](../specs/publication/rem-encrypt-1-specification.md): the
  encrypted representation and its X-Wing hybrid key-encapsulation profile
  (X25519 and ML-KEM-768).
- [REM-PARITY 1.0](../specs/publication/rem-parity-1-specification.md): the
  Reed-Solomon tape layout, sidecars and recovery from the bootstrap without a
  catalog, in the published copy.
- [Architecture overview](architecture-overview.md): the layer stack, the write
  and read paths, the catalog, the audit log, the tape logs and the daemon.
- [Partial-file restore](pfr-reference.md): how one byte range of a file is read
  from tape, with a comparison with LTFS extents.
- [Installing Remanence](guide-install.md): building from source and preparing a
  host.
- [README](../README.md): the project overview and the shortest way to try it.

In plain terms: Remanence is the part of an archive that deals with the tape
and nothing else. It keeps track of which drive is which, refuses to guess when
the hardware leaves the outcome of a command in doubt, and writes tapes in a
published format that can be read without it. Everything about what the archive
is for stays with the system that calls it.
