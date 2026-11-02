# Importing and recovering Remanence tapes

This guide is for the operator of a tape library who holds a Remanence
cartridge that the local catalog does not know. Typical cases are a catalog
lost in a host failure, a cartridge returning from an offsite vault to a
replacement host, and a cartridge moving from one Remanence site to another.
The guide shows how to identify such a tape without writing to it, how to
record its identity in the catalog, and how to find out what it holds. It also
says which tapes cannot be imported this way, and what to do with them instead.

A few terms recur. A **tape file** is the run of blocks between two filemarks,
the separators a drive records on the medium. A **block** is the fixed-size
record the drive reads and writes; its size is chosen when the tape is
initialized. **BOT** is the beginning of tape and **EOD** the end of recorded
data. The first tape file is the **bootstrap**, which records the tape UUID and
how the tape was written. An **object** is the unit Remanence stores: one tape
file that holds an archive of files. The **tape log** is the host's record of
what was committed to one tape, kept in the state directory, and the
**catalog** is the SQLite database that Remanence rebuilds from the tape logs
and the audit log. [Concepts](concepts.md) introduces these terms in order, and
the [glossary](reference-glossary.md) has the rest.

## How import works

The bootstrap works like an identity card that travels with the cartridge. The
catalog is the site's card index: it records which tapes and objects this site
knows. Losing the catalog, or moving a cartridge to another site, removes the
card-index entry and leaves the identity card on the tape. **Adoption** reads
the identity card and creates a minimal local record for the tape. It does not
initialize the tape and writes nothing to it.

Import happens in three separate stages:

| Stage | Command | What Remanence learns | What it does not assume |
| --- | --- | --- | --- |
| Probe | `rem-debug tape bot-probe` | Whether BOT holds a valid Remanence bootstrap, with the tape UUID and geometry. | That the catalog is correct, or that the rest of the tape is healthy. |
| Adopt | `rem-debug tape adopt-bootstrap` | The tape's identity, barcode, pool and geometry, and a lifecycle state taken from what follows the bootstrap. | Which objects are on the tape, or whether they were committed. |
| Inventory and verify | `rem tape inventory`, `rem tape verify-index` | The contents, read from the terminal index or from a walk from BOT. A full verification also reads every protected data block and parity shard. | That an inventory has rebuilt the catalog. |

The stages are separate so that Remanence can recognize and inspect a cartridge
without inventing a history for it. The **terminal index** is the list of every
object on the tape, written three times (replicas A, B and C) at the end of the
tape when it is finalized. A finalized tape offers a fast read of it. A walk
from BOT is needed only when no replica can be used, as on a tape that was never
finalized or one whose replicas cannot be read.

Two limits shape the rest of this guide. The first is that adoption records
identity only. Remanence does not turn the rows of a tape-only inventory into
ordinary catalog entries: `rem rebuild-catalog-from-journals` rebuilds the
catalog from surviving tape logs and the audit log, and it reads neither a
terminal index nor a walk. A site that holds only the cartridge can identify it
and inspect it, and it can restore files from it in a standalone drive, but it
cannot import the tape's objects into its catalog. The second limit is that
adoption requires every drive in the library to support the tape's recorded
block size, and the bootstrap must record drive compression off. [Which tapes
can be adopted](#which-tapes-can-be-adopted) explains the rule and the alternatives.

## What "foreign tape" means

"Foreign" can describe quite different cartridges, and only some of them are
handled here:

| Cartridge | Handled by this guide? | What to do |
| --- | --- | --- |
| A Remanence tape whose catalog row was lost, on a host whose state directory survived | Not needed | Rebuild the catalog from the tape logs ([Rebuild the catalog](#rebuild-the-catalog-from-the-tape-logs)). This works for every geometry. |
| A Remanence tape written at another site | Identity only | Probe and adopt it, then inventory and verify it. Its objects can be listed, and restored in a standalone drive, but not imported into this catalog. |
| A Remanence tape whose block size a library drive cannot support, or whose bootstrap records compression on | No | See [Which tapes can be adopted](#which-tapes-can-be-adopted). |
| A Remanence tape with a damaged or unreadable bootstrap | No | Do not adopt it. [Damaged tapes](guide-damaged-tapes.md) describes `rem-debug tape recovery-report`, which reads such a tape with values you supply. |
| An LTFS, tar or other non-Remanence tape | No | Stock Remanence includes no parser for other formats. [Foreign-format adapters](reference-foreign-format-adapters.md) describes the read-only adapter interface that separately versioned distributions can use. |
| A blank tape | No | Initialize it as new media with `rem tape init`. |

Adoption recognizes an identity that is already on the tape. It never converts
another format into a Remanence tape, relabels a cartridge, overrides a retired
identity, or makes an already written tape writable without its real tape log.

For example, site A writes a Remanence tape and sends it to an offsite vault.
Site B later receives the cartridge without site A's state directory. Site B
can probe and adopt the bootstrap identity, then inventory and verify the tape.
It can restore files from the tape in a standalone drive, which needs no
catalog. Site B cannot turn the inventory into catalog entries, so `rem list`
and `rem get` on site B do not know the tape's objects. An LTFS cartridge in
the same shipment is rejected by the stock tools as a foreign or unrecognized
format.

For a transfer between sites, the destination must support the tape's bootstrap
and on-tape format versions. Its configuration also needs a pool rule that maps
the barcode to a configured pool. Adoption does not decrypt or restore objects,
and encrypted objects still need their private keys. A WORM (write-once)
cartridge is safe for this read-only identity step, but Remanence does not write
ordinary objects to WORM media.

<!-- code-anchor: crates/remanence-api/src/tape_init.rs crates/remanence-state/src/state.rs crates/remanence-state/src/config.rs crates/remanence-cli/src/lib.rs crates/remanence-cli/src/standalone.rs @ 625f7044 -->
## Which tapes can be adopted

`rem-debug tape adopt-bootstrap` accepts a bootstrap only when all of these
hold:

- the tape UUID is an RFC 4122 version 4 UUID;
- the recorded block size is valid and every drive in the library supports it;
- the recorded parity scheme is valid, including no parity;
- the bootstrap records drive compression off.

The probe reports valid geometry with compression off in its
`canonical_adoption_geometry` field. Adoption checks the recorded geometry in
the command and again when the catalog records the identity; the command also
checks the block-size limit of every drive in the library. The probe's field
does not establish that every library drive supports the block size.

Remanence's own defaults produce adoptable geometry. In a library, `rem tape
init` takes the block size from the tape's pool, which is 256 KiB unless the
pool's `block_size` key says otherwise, and it writes no parity unless it is
given `--parity default`. A home-mode pool uses 256 KiB blocks and the parity
scheme `custom:128,4,1`. These geometries can all be adopted when every library
drive supports the recorded block size. Adoption records the tape's own
geometry; the destination pool's initialization defaults do not replace it.
The [configuration reference](reference-configuration.md) describes the pool
block-size key.

Adoption records identity only. Other recovery options depend on what survives:

- If this host's state directory survived, or you can restore it from a backup,
  and only the catalog was lost or damaged, you do not need adoption. Stop the
  daemon and rebuild the catalog ([Rebuild the catalog](#rebuild-the-catalog-from-the-tape-logs)).
  The rebuild recreates the tape's row and its objects from the audit log and
  the tape log, whatever the tape's geometry.
- To see what the tape holds without a catalog, load it into a drive and run
  `rem-debug tape recovery-report` on the drive's device
  ([Inspect a tape without the catalog](#inspect-a-tape-without-the-catalog)).
  The report never reads or writes the catalog.
- To restore files, read the tape in a standalone drive, one that is not inside
  a library. There, `rem tape restore` lists and restores objects from the
  terminal index, or from a scan, without any catalog, and `rem tape adopt`
  records the identity of a tape of any valid geometry in that host's catalog.
  [Recovery on another machine](guide-standalone-drive.md#recovery-on-another-machine)
  shows the commands. The standalone commands refuse a drive that belongs to a
  library, with `device is not an unattached drive; library operations go
  through the daemon`.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-api/src/tape_init.rs @ 625f7044 -->
## How the commands treat the tape

Both commands read the tape and neither writes to it, but they read it in
different ways:

1. `bot-probe` is advisory. It resolves one exact barcode in one configured
   library, checks that the cartridge sits in the home slot you name, moves it
   into one drive that can read its generation, and reads BOT with exactly one
   LOCATE to block 0 (the SCSI command that positions the tape at a block
   address) and at most one READ. It does not consult the catalog.
2. `adopt-bootstrap` does not rely on the earlier probe. It takes the exclusive
   state lock, runs a fresh discovery, checks the barcode, library, home slot and
   drive again, and rereads the bootstrap and what immediately follows it under
   the same drive handle. The UUID you pass is compared with the UUID from this
   fresh read, not with discovery or catalog data.

Both commands set the drive to variable-block mode with compression off for the
read, check that the drive accepted the setting, and afterwards restore and
check the drive's previous mode. They then return the cartridge to the exact
home slot. A failure to restore the mode or to return the cartridge fails the
command, and no catalog change is made. Before reading, both wait for the drive
to report the cartridge ready, for up to 9,000 seconds (2.5 hours), because a
first load of LTO-9 media can take an hour or more while the drive optimizes
it.

The state lock is a local advisory lock on the state directory, and the daemon
holds it for its whole lifetime, so adoption refuses to run beside a running
daemon with `state lock is already held`. The lock excludes only cooperating
Remanence processes. It is not a SCSI reservation, so it cannot stop other
robotics or tape software from moving the same cartridge. Stop the daemon before
you probe or adopt, and stop every other system that could move or use the same
cartridge or drive.

## Probe the tape

To probe the cartridge `RMJ101L9` in slot `0x0400` of library `LIB0001234`:

```sh
rem-debug --allow LIB0001234 tape bot-probe RMJ101L9 \
  --library LIB0001234 --expected-home-slot 0x0400 --json
```

Replace the three values with your own. `LIB0001234` is the library's serial:
`rem libraries` prints it, and the same value goes to `--allow` (the libraries
this invocation may operate) and to `--library`. `RMJ101L9` is the cartridge's
barcode and `0x0400` the element address of the storage slot that holds it;
`rem library LIB0001234 --slots` lists both. The address may be given in hex or
decimal.

The probe prints one line of JSON in schema `rem.tape.bot-probe.v1`. Formatted,
for a tape written with the adoptable geometry, it looks like this (the values
are illustrative):

```text
{
  "schema": "rem.tape.bot-probe.v1",
  "ok": true,
  "physical_disposition": "bootstrap_valid",
  "library_serial": "LIB0001234",
  "library_revision": "1.0",
  "barcode": "RMJ101L9",
  "source_slot": 1024,
  "drive_element": 1,
  "drive_serial": "HU2345BCD6",
  "configured_drive_compression": false,
  "bootstrap_drive_compression": false,
  "tape_uuid": "5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f",
  "geometry": {
    "block_size_bytes": 1048576,
    "drive_compression": false,
    "no_parity": false,
    "scheme_id": "rs-cauchy-gf256-v1",
    "data_blocks_per_stripe": 128,
    "parity_blocks_per_stripe": 4,
    "stripes_per_neighborhood": 128
  },
  "canonical_adoption_geometry": true,
  "detail": null
}
```

Element addresses are printed in decimal: `source_slot` 1024 is `0x0400`. Check
that `library_serial`, `barcode` and `source_slot` are the ones you expected,
that `canonical_adoption_geometry` is true, and note the `tape_uuid`. The other
values of `physical_disposition` mean:

| `physical_disposition` | What the probe found | Next step |
| --- | --- | --- |
| `bootstrap_valid` | A checksum-valid Remanence bootstrap whose recorded block size matches the record read. `tape_uuid` and `geometry` are filled in. | Adopt it if `canonical_adoption_geometry` is true; otherwise see [Which tapes can be adopted](#which-tapes-can-be-adopted). |
| `bootstrap_damaged` | The bootstrap's magic number is present, but its framing or checksum fails, its UUID is not version 4, or its recorded block size differs from the record read. `detail` says which. | Do not adopt it. See [Damaged tapes](guide-damaged-tapes.md). |
| `foreign_format` | BOT matched a known non-Remanence format, which `detail` names. | Do not adopt it. Read it with a foreign-format adapter. |
| `unrecognized_data` | BOT holds data with no signature Remanence recognizes. | Do not adopt it. |
| `blank_check_eod` | The drive reported blank media at BOT. | Initialize it as a new tape if you mean to use it. |
| `read_error` | Positioning or reading BOT failed, so the identity is unknown. `detail` gives the error. | Check the drive and the cartridge, then probe again. |

## Adopt the tape

To adopt the tape, give the same library, barcode and home slot, and the
`tape_uuid` from the probe:

```sh
rem-debug --allow LIB0001234 tape adopt-bootstrap RMJ101L9 \
  --library LIB0001234 --expected-home-slot 0x0400 \
  --expected-existing-tape-uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f --json
```

Take the UUID from the probe's output, not from an old catalog. Adoption refuses
if its own read of BOT finds a different UUID. The command generates its own
operation UUID, so there is no receipt to create or keep.

Adoption prints one line of JSON in schema `rem.tape.adopt-bootstrap.v1`. It
repeats the probe's identity fields and adds what the adoption did. Formatted
and trimmed, with illustrative values:

```text
{
  "schema": "rem.tape.adopt-bootstrap.v1",
  "ok": true,
  "physical_disposition": "bootstrap_adopted",
  "physical_tail": "exact_bootstrap_filemark_eod",
  "newly_adopted": true,
  "operation_id": "c41f7a2e-9b3d-4e61-a8f5-2d7c0b9e1a34",
  "idempotency_fingerprint": "5f6ab7e1adf13040b0f95c3c1109e88c1ae4b7379a536afc4f4ea092b136485b",
  "barcode": "RMJ101L9",
  "tape_uuid": "5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f",
  ...
  "catalog": {
    "tape_uuid": "5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f",
    "voltag": "RMJ101L9",
    "kind": "data",
    "pool_id": "archive",
    "state": "ready",
    "assignment_generation": 1,
    "geometry": { ... }
  }
}
```

`physical_tail` describes what adoption found right after the bootstrap, and it
decides the state the tape is given in the catalog:

| `physical_tail` | What follows the bootstrap | Catalog state |
| --- | --- | --- |
| `exact_bootstrap_filemark_eod` | One filemark, then EOD: the tape holds nothing but its bootstrap. | `ready` |
| `data_after_bootstrap` | A data record, either in place of the bootstrap's filemark or after it. | `recovery_required` |
| `missing_filemark` | EOD straight after the bootstrap, without its filemark. | `recovery_required` |
| `extra_filemark` | A second filemark after the bootstrap's filemark. | `recovery_required` |
| `ambiguous` | The next boundary could not be proved. | `recovery_required` |

`recovery_required` does not mean that the identity is damaged. It means the
identity was recovered, and the tape has more, different or uncertain content
that must be inventoried before the tape is used. Writes accept only tapes in
state `ready`, so a `recovery_required` tape is kept out of write service.

`pool_id` comes from this site's configuration: the `[[tape_pool_rules]]` entry
with the longest prefix that matches the barcode, as for any other tape. Check
that the cartridge was returned to its slot and that the `catalog` state is the
one you expected.

## What adoption records

A first adoption writes a `TapeIdentityAdopted` record to the audit log and
forces it to stable storage (fsync) before it writes the catalog row. The row
holds the tape UUID, barcode, geometry, pool, lifecycle state and assignment
generation. Adoption creates no tape-file rows, no object copies, no committed
position, no session and no finalization record. For a tape that holds exactly
a bootstrap, that is enough to use it as an empty Remanence cartridge. For any
other tape, the catalog knows which tape it is and nothing about its contents.

## Running adoption again

Each run of `adopt-bootstrap` generates a new operation UUID. A repeat run
against a tape that is already adopted succeeds without a second audit record
when the catalog row is still the identity-only row that adoption made: the same
UUID, barcode, geometry, pool, assignment generation and state, with no tape
files, objects, open session, active fence or finalization. Its output then
reports `"physical_disposition": "bootstrap_already_adopted"` and
`"newly_adopted": false`.

If the process stops after the audit record is written and before the catalog
row, the next run replays the audit record and completes the row. Once writes or
lifecycle changes have changed the row, a new adoption is refused and does not
roll the state back.

## After adoption

For a tape reported `ready`, there is nothing more to do; it is an empty
cartridge in its pool.

For a `recovery_required` tape, start the daemon and read the tape's contents
through it:

```sh
rem tape inventory --tape-uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f --json
rem tape verify-index --tape-uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f --json
```

Both commands take the tape UUID; neither accepts a barcode, and neither has a
depth option.

`rem tape inventory` is the quick check. It reads the bootstrap at BOT and the
terminal index replicas at the end of the tape, in a fixed fallback order. When
a replica is usable, it reads that replica's list of objects and reads no
object data. It does not walk the tape between the bootstrap and the terminal
index, so it does not prove that the middle of the tape matches the list. If no
replica is usable, it reports that explicitly and falls back to a walk from BOT,
which measures every tape file and reads selected blocks of each object.

A walk can run for hours, so the inventory's event stream
(`rem.tape.inventory.stream.v1`) announces `bot_recovery_started` before it
begins. It then reports one `bot_recovery_progress` event after each tape file,
with the tape-file number, the partition and logical block address (LBA), the
object candidates found so far and the elapsed milliseconds. The human output
shows the same facts on `bot_recovery:` and `bot_recovery_progress:` lines.
Cancellation is checked at each of these points and stops before the next tape
file is read. Use a daemon and a CLI from the same build to get these events. A
walk can also start on a tape whose three replicas are intact, when something
was written after them; [Damaged
tapes](guide-damaged-tapes.md#the-terminal-index-is-damaged-or-is-not-where-it-was-expected)
explains why and how to read what the walk reports.

`rem tape verify-index` is the full check. It reads every data block that a
parity sidecar protects and every parity shard, checks each against the
sidecar's index, and reports each failure by its address. It also measures the
tape files before the terminal index and checks the terminal structures. For a
full 18 TB LTO-9 cartridge written with parity, at the half-height drive's
native 300 MB/s, reading the bytes alone takes 60,000 seconds, or 16 hours and
40 minutes, and repositioning adds to that. [Full
verification](guide-full-verification.md) explains its output and how to
estimate its duration.

Inventory rows are inspection evidence, and the inventory does not add them to
the catalog. Remanence does not create a tape log from an inventory either, so a
`recovery_required` tape adopted from another site stays in that state, out of
write service.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-state/src/state.rs crates/remanence-state/src/tape_log/projection.rs @ 625f7044 -->
## Rebuild the catalog from the tape logs

When this host's own state directory holds the tape logs and the audit log, the
catalog can be rebuilt from them, and no adoption is needed. The same applies
after you restore the state directory from a backup. The tape logs live in the
directory named by the `dir` key of the configuration's `[journal]` table; the
command keeps its older name.

Stop the daemon, restore the state directory to its configured locations if it
came from a backup, and run:

```sh
rem rebuild-catalog-from-journals --config /etc/rem/config.toml
```

`/etc/rem/config.toml` is the daemon's configuration file and the default for
`--config`. The rebuild takes the state lock, so it refuses to run beside a
running daemon. It replaces every row it can derive and leaves the tape logs and
the audit log unchanged. It prints a summary of the form below (illustrative
counts):

```text
rebuild-catalog-from-journals ok
  tapes: 3
  tape files: 214
  object copies: 211
  audit records: 1840
  tape logs: 3
```

Warnings go to standard error. A line that begins
`WARN audit-replay-divergence:` names an audit record that could not be
replayed cleanly, or a value on which the audit log and the catalog's earlier
contents disagreed, and ends `ledger value applied`: the audit log's value was
used. A rebuild in a state directory without an
`authority-format` marker refuses with `invalid config: projection rebuild
requires an existing authority-format marker`.

<!-- code-anchor: crates/remanence-cli/src/recovery_report.rs crates/remanence-cli/src/lib.rs @ 625f7044 -->
## Inspect a tape without the catalog

`rem-debug tape recovery-report` reads a tape without the catalog, the daemon
or any host state. It finds the bootstrap at a block size of 256 KiB, 512 KiB or
1 MiB, selects a terminal index replica or walks the tape, and checks each
object it finds. It works on a tape of any geometry, adopted or not. It does not
load the cartridge itself, so load it first and unload it afterwards, with the
daemon stopped. To read `RMJ101L9` from slot `0x0400` in the drive at bay
`0x0001`:

```sh
rem-debug --allow LIB0001234 load LIB0001234 --slot 0x0400 --bay 0x0001
rem-debug tape recovery-report /dev/sg6 --json
rem-debug --allow LIB0001234 unload LIB0001234 --bay 0x0001
```

`/dev/sg6` stands for the drive's SCSI generic device, which
`rem library LIB0001234` lists beside each drive bay. `unload` returns the
cartridge to the slot it came from unless you give `--dest`. Without `--json`,
the report prints one `object tape-file N ...` line for each object it found,
with its map and verification status, then a `totals:` line and
`result: verified` or `result: failed`. If the bootstrap itself cannot be read,
the report needs the tape UUID, block size and parity scheme from your records;
[Damaged tapes](guide-damaged-tapes.md#the-bootstrap-is-unreadable) explains
how to supply them.

<!-- code-anchor: crates/remanence-parity/src/bot_recovery.rs crates/remanence-state/src/authority/bot_recovery.rs crates/remanence-cli/src/tape_inventory.rs @ 625f7044 -->
## How object names are recovered when the terminal index is damaged

A healthy finalized tape carries its complete list of objects in each of its
three replicas. If all three are unusable, the walk from BOT can show that a
tape file is a complete object, but a tape file's position does not reveal the
object's name.

The walk therefore uses this host's tape log for the tape, when one exists. The
tape log's commit record for an object is forced to stable storage only after
the object's body and trailing filemark are durable on tape. During the walk,
Remanence replays the log without loading every object row into memory. It
accepts a name only when all of these agree:

- the log is for the same tape UUID and fixed block size;
- the log's committed tape files are all present in the measured tape;
- every logged object names the tape-file number and block count that the walk
  measured;
- log rows and measured objects match one to one across the committed tape
  files.

Each object candidate is then printed on a `bot_object:` line with one of three
states:

| State | Meaning |
| --- | --- |
| `recovered` | The object's tape file is physically complete, and an exact row of the tape log supplied its object identifier. |
| `unknown` | The object's tape file is physically complete, but no trustworthy record supplies its identity. |
| `incomplete` | EOD was reached before the object's trailing filemark. No name from the tape log is applied to it. |

After a crash, a complete object can lie on tape beyond the last commit the tape
log recorded. It is reported as `unknown` and is not promoted to `recovered`. A
corrupt tape log, or one whose geometry conflicts with the tape, stops the
operation with a data-loss error rather than being ignored.

A tape from another site has no tape log on the receiving host, and Remanence
does not create one because an inventory was requested. If all its replicas are
invalid, its complete objects are therefore reported as `unknown`, and
`rem-debug tape recovery-report` reports them as `<identity unavailable>`. Each
object still records its own identifier inside it, and `rem tape restore --scan`
on a standalone drive reads it from there. In either case the results are
inspection evidence and do not enter the catalog.

## Crash recovery and retries

This section is background for an operator whose own host was interrupted while
writing or finalizing a tape. It also explains why a cartridge from another site
cannot be appended to after it is adopted.

### Interrupted object writes are retried whole

An object is one complete REM-OBJECT tar stream, possibly holding many
separately named and checksummed files. Remanence writes the filemark only after
all of the object's blocks have been written, so a power cut never leaves "part
one, filemark, part two". Members inside a bundle have no tape filemarks between
them.

If the host stops while writing the body, the tail may hold some physical
blocks but has no committed object delimiter and no catalog entry. On an open,
parity-off, rewritable tape, the last durable tape log records the
EOD after the previous complete object. After a restart, a new write session
positions at that checkpoint EOD, and the caller sends the interrupted object
again from byte zero. The rewrite replaces the uncommitted tail, and only the
completed retry gets a filemark and enters the catalog. A stock tar
implementation therefore still sees one complete tar stream for the object, not
two pieces that need Remanence-specific joining.

This cannot be done in place on WORM media, where the incomplete tail cannot be
erased. Remanence admits ordinary object sessions only when the drive reports
`NotWorm`; `Worm` and `Unknown` are both refused before the drive mode is
changed. Read-only adoption and recovery are safe on WORM, and terminal-index
finalization has its own append-only recovery path for WORM. Remanence does not
implement WORM object ingest.

The caller keeps the source until `CheckpointSession` reports the object
committed. Resuming a session is not the recovery method. An interrupted
session is abandoned, and every object that was not checkpointed is replayed in
full with the same caller id, input kind, content digest, archived logical-file
path, and copy intent. Copy intent is plaintext versus encrypted and, for
encrypted copies, the exact ordered recipient epoch ids. If the catalog already
holds that exact request, the retry reads back the matching committed copy
(an idempotent retry). Changing any of these fields under the same caller id is
refused. For a canonical REM-OBJECT, the bytes already bind every member path,
so the outer `archive_path` is not part of the replay identity.

The canonical append surface creates an object and its first copy as one
checkpointed operation. Media completion can come before the checkpoint and
catalog commit, which is why an interrupted, uncommitted tail can exist and must
be rewritten whole. The surface does not attach another copy to an object UUID
that is already cataloged outside the exact pool and caller replay key; that
conflict is refused before any tape motion. A daemon-wide claim holds the pool
and caller key and the canonical object UUID from admission, before any motion,
through the checkpoint. Two drive actors therefore cannot both write the same
identity.

The one-shot direct writer follows the same rule under the exclusive state
lock. Before it selects a tape, it replays every tape log in the configured
tape-log directory (`journal.dir`), not only the log of the tape it might
select, and resolves an exact committed retry from host state alone. A
committed retry therefore needs no writable tape and moves no cartridge. This
also covers the case where tape A has a durable checkpoint but SQLite has not
yet recorded the object identity: a retry cannot select tape B and write the
same identity again. The drive-bound writer repeats the reconciliation and
replay check. It takes the pool policy, tape-log directory, and memory ceiling
from the locked operator configuration, not from its caller. Host-only
encrypted replay reports the committed ordered epoch ids. The host catalog does
not keep the authenticated on-tape labels, so those labels are reported as
unknown instead of being copied from the new retry request.

If log publication fails with uncertain authority, Remanence poisons the writer
and requires physical-tail reconciliation before retrying the affected WRITTEN
objects. A later process must reconcile the log before admitting a retry. A
validated closed group is projected into the catalog. A torn trailing frame
is not treated as committed. If the log is durable but projection failed,
replay repairs SQLite without writing the object again.

This mechanism depends on the host tape log of an open tape. A
cartridge arriving from another installation does not gain safe append
authority just because its bootstrap can be adopted. Transfer the matching host
tape logs when they are available. Otherwise keep the cartridge out of ordinary
write service and use terminal inventory or the bounded BOT recovery to find
out what is on it. Adoption alone never guesses the last committed append
position.

A drive or transport failure can leave the completion of the last command
uncertain. Remanence then fences that tape and does not assume how much was
written. The whole-object rule still applies, but an operator must first
reconcile or release the fence using the recorded evidence. The project's
end-to-end recovery test on a virtual tape library exercises an abrupt daemon
loss after a completed write command. It does not emulate the volatile-cache
behavior of any particular physical drive.

### Finalization recovery

When a tape is finalized, the writer closes the open parity epoch and writes
the three terminal replicas, A, B, and C, with a separation extent (a control
file with a header, zero-filled interior records, and a footer) between each
pair. If the tape log proves that replica C completed its synchronizing
barrier, all three full indexes are already on tape. A restart then completes
the remaining bookkeeping without loading, locating, reading, or writing the
cartridge. That bookkeeping is the sealed log record, the SQLite projection and
the audit entries. It applies to automatic finalization and to an
operator-requested close-out.

During finalization the tape log records the decision, immutable plan and
barrier-proved component progress. An ordinary write session cannot append an
object after the first durable finalization transition. Recovery validates the
plan and projects durable progress before any terminal media motion.

If the operation was already classified `recovery_required`, that state stays
until recovery proves the next component or records the completed seal. The
`Sealed` log record carries the completed intent. A restart replays it into
SQLite. Changes to a pool's capacity cap or watermarks do not block host-only
completion after replica C, because the complete reserved tail is already on
the tape.

The same restart repairs missing completion records in the append-only audit
log. It records one tape-sealed event and, for a manual close-out, one
operation-finished event. An event written before the crash is reused and not
duplicated. Both records are always flushed to stable storage. Only one daemon
or offline state owner can use a state directory at a time, so another process
cannot race that decision.

Before the replica C boundary, a failed or uncertain component is recorded
durably as `recovery_required`. A restart does not turn that state back into
ordinary `finalizing`. It reconciles the next physical component first. A host
metadata failure after a proved replica C therefore causes no needless tape
motion, while genuinely uncertain media stays fenced.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-state/src/state.rs crates/remanence-state/src/index.rs crates/remanence-state/src/lock.rs @ 625f7044 -->
## Refusals to expect

Each refusal is printed on standard error after `error:`, and the command
exits with status 1. No refusal changes the catalog. Values in capitals stand
for the ones in your command or on your tape. The messages from the catalog
begin with `tape provisioning conflict:`, print a barcode as `Some("BARCODE")`,
and print a tape UUID as 32 hex digits without hyphens (`HEX`).

| Message | What it means | What to do |
| --- | --- | --- |
| `state lock is already held: PATH` | The daemon, or another offline command, holds the state directory. | Stop the daemon and wait for other offline commands to finish. |
| `barcode "BARCODE" was not found in configured discovered libraries` | No configured library reports that barcode. | Check the barcode and the library's configuration; `rem library SERIAL --slots` lists the slots. |
| `barcode "BARCODE" is ambiguous; pass --library or an element address` | More than one place in the library holds that barcode. | Remove the duplicate cartridge, then run again. |
| `barcode ... has no known LTO generation suffix` | The barcode does not end in a media code such as `L9`, so no compatible drive can be chosen. | Use a cartridge whose barcode ends in its media code. |
| `command expected home slot 0xEXPECTED, but live inventory resolved 0xACTUAL` | The cartridge is not in the slot you named. | Check `rem library SERIAL --slots` and give the slot that holds it. |
| `media in drive 0xBAY is not ready: ...` | The drive did not report the cartridge ready before the wait ended. | Check the drive and the cartridge before trying again. |
| `tape adopt-bootstrap is blocked by active media-readiness fence ID ...` | An earlier readiness wait left a fence on the drive or the cartridge. | Inspect it with `rem tape quarantine show ID`, as the message says. |
| `bootstrap adoption requires drive compression disabled: this tape was written with drive compression on` | The bootstrap records drive compression on. Remanence writes with drive compression off and cannot adopt such a tape. | See [Which tapes can be adopted](#which-tapes-can-be-adopted). |
| `valid Bootstrap has invalid adoption geometry` | The bootstrap records an invalid block size or parity scheme. | See [Which tapes can be adopted](#which-tapes-can-be-adopted). |
| `drive 0xBAY maximum block size MAX is smaller than Bootstrap block size SIZE` | A library drive cannot use the tape's recorded block size. | Use a library whose drives all support that size. |
| `BOT is not adoptable: InvalidBot(...)` | The fresh read found no valid bootstrap; the text in parentheses gives the classification. | Probe the tape again and follow the table for its `physical_disposition`. |
| `command expected tape UUID UUID, but BOT contains UUID` | The tape's UUID is not the one you passed. | Probe again and pass the UUID it reports. |
| `tape provisioning conflict: bootstrap barcode Some("BARCODE") does not match any locked tape_pool_rule` | No `[[tape_pool_rules]]` entry matches the barcode. | Add a rule for it to the configuration. |
| `tape provisioning conflict: bootstrap barcode Some("BARCODE") derives unconfigured pool "POOL"` | The matching rule names a pool that has no `[[tape_pools]]` table. | Configure that pool. |
| `tape provisioning conflict: Bootstrap identity HEX is retired; retired identities are permanent` | This catalog retired the tape's identity. | A retired identity cannot be adopted again. |
| `tape provisioning conflict: Bootstrap UUID HEX and barcode Some("BARCODE") resolve to different catalog identities` | The catalog knows the UUID and the barcode as two different tapes. | Resolve the existing rows before adopting. |
| `tape provisioning conflict: Bootstrap identity HEX barcode Some("BARCODE") conflicts with existing KIND catalog row` | The catalog already has a row for this tape that adoption did not make, or one that has changed since. | Do not adopt again; the existing row is the tape's authority. |

A failure to restore the drive's mode or to return the cartridge to its home
slot also stops the command before any catalog change. The message then begins
with `restore prior drive config:` or `park tape from drive`.

## Checklist

1. Stop the daemon, robotics, backup software and any other owner of the drives.
2. Confirm that the library is configured and that the barcode appears exactly
   once, and note the element address of the cartridge's slot.
3. Confirm that a `[[tape_pool_rules]]` entry maps the barcode to a configured
   pool.
4. Run `bot-probe`. Require `bootstrap_valid`, `canonical_adoption_geometry`
   true, and the library, barcode and slot you expected. For any other result,
   do not adopt; follow [Which tapes can be adopted](#which-tapes-can-be-adopted)
   or [Damaged tapes](guide-damaged-tapes.md).
5. Run `adopt-bootstrap` with the UUID from the probe's output and the same
   library and slot.
6. Confirm that the cartridge is back in its slot, and read the `catalog` state
   in the output.
7. If the state is `recovery_required`, start the daemon and run
   `rem tape inventory`, then `rem tape verify-index` if you need the full
   check. The tape stays out of write service.
8. If this host's state directory, or a backup of it, holds the tape's tape log,
   stop the daemon again and rebuild the catalog from it. If there is no tape
   log, do not try to build one from the inventory: restore what you need with
   the catalog-less tools and keep the tape out of write service.
9. Return the library to normal operation once the cartridges and the catalog
   agree.
