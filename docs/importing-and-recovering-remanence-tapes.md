# Importing and recovering Remanence tapes

This guide explains what Remanence does when it meets a tape that its local
catalog knows nothing about, and what an operator does about it. Typical cases
are a catalog lost in a host failure, a cartridge returning from an offsite
vault to a replacement host, and a cartridge moving from one Remanence site to
another. The guide covers how Remanence recognizes such a tape without writing
to it, what it records, what it deliberately does not record, how an
interrupted write is recovered, and the commands to run.

A few terms are used throughout. A **tape file** is the stretch of a tape
between two **filemarks**, the separator marks a drive records on the medium.
**BOT** is the beginning of tape and **EOD** the end of recorded data. A
**block** is the fixed-size record the drive reads and writes, 1 MiB here. An
**Object** is the unit Remanence stores: one archive holding one or more files
and a manifest, written in the REM-OBJECT format. The **catalog** is the SQLite
database in which a Remanence installation records which tapes and Objects it
knows. **Parity** is extra data written to the tape so that damaged blocks can
be reconstructed. More terms are in the [glossary](reference-glossary.md).

## The short version

A Remanence tape carries its own identity card. The first record on the tape,
called the **bootstrap**, says which tape this is and how it was written. A
Remanence installation also keeps a local SQLite catalog, which works like the
site's card index: it records which tapes and Objects that site knows about.

Moving a cartridge to another site, or losing the SQLite database, removes the
card-index entry but leaves the identity card on the tape. **Adoption** reads
that on-tape identity and creates a minimal, conservative local record for the
tape. It does not initialize the tape and does not write anything to it.

Import and recovery happen in three separate stages:

| Stage | What Remanence learns | What it does not assume |
|---|---|---|
| Probe | Whether BOT holds a valid Remanence bootstrap, with the tape UUID and geometry | That the local catalog is correct, or that the rest of the tape is healthy |
| Adopt | The tape's identity, barcode, pool, geometry, and a conservative lifecycle state | Which Objects are present, or whether they were committed |
| Inventory and verify | The contents and the structural evidence readable from the terminal indexes or from a scan starting at BOT. A full verification also reads every protected data block and parity shard | That a read-only inventory has rebuilt the site's writable catalog |

The stages are separate so that Remanence can recognize and inspect a cartridge
without inventing a history for it. The **terminal indexes** are three copies of
the complete Object inventory, written at the end of a tape when it is
finalized. A finalized tape normally offers a fast read of them. A complete scan from the
beginning of the tape is needed only when terminal discovery finds no usable
replica, as on a partially written tape or one whose terminal indexes are
unreadable. A finalization that was interrupted can leave replica A, or A and B,
usable.

Remanence does not turn tape-only inventory rows into the ordinary Object,
copy, and file catalog. `rebuild-catalog-from-journals` replays surviving host
audit logs and per-tape journals; it does not read a terminal index or a BOT
scan. A site that holds only a cartridge can identify and inspect it, but
Remanence does not yet import its contents into the catalog.

## What “foreign tape” means

“Foreign” can describe two quite different situations:

| Cartridge | Supported by this workflow? | Correct path |
|---|---:|---|
| A Remanence tape written at another site | Partly | Probe and adopt its identity, then inventory and verify. Ordinary catalog import additionally needs the source site's host journals |
| A Remanence tape whose local SQLite row was lost | Yes | The same probe-and-adopt workflow |
| A Remanence tape with a damaged or unreadable bootstrap | No | Do not adopt; preserve the cartridge for a deeper recovery procedure |
| An LTFS, tar, or other non-Remanence tape | No | Use a separately supplied, read-only foreign-format adapter |
| A blank tape | No | Initialize it as new media |

The supported case is a cartridge that is foreign to this installation but
native to Remanence. The adoption command recognizes the identity already on
the tape. It does not convert another tape format into Remanence.

Stock Remanence includes no parser for LTFS, tar, or other
formats. Its core exposes the adapter interface, and separately versioned
distributions may link read-only adapters. [Foreign-format
adapters](reference-foreign-format-adapters.md) describes that path and what is
currently available.

Adoption restores identity only. It does not tell the site's catalog about the
tape's Objects, files, committed prefix, terminal indexes, or finalization
history.

For example, Site A writes a Remanence tape and sends it to an offsite vault.
Site B later receives the cartridge without Site A's SQLite catalog. Site B can
probe and adopt the bootstrap identity, then inventory and verify the terminal
indexes. If Site A also sent its audit and per-tape journals, Site B can
rebuild the ordinary catalog from those host records. With the cartridge alone,
Site B can inspect the tape but cannot import its inventory rows as ordinary
catalog entries. An LTFS cartridge in the same shipment needs a distribution
with an LTFS adapter. The stock Remanence tools reject it as unrecognized or
unsupported.

## How Object names are recovered when the terminal indexes are damaged

A healthy finalized tape carries its complete Object inventory in each of the
three terminal replicas. If all three are unusable, Remanence scans every
tape file from the beginning, splitting them at the filemarks. The scan can
show that an Object-shaped tape file is complete, but the position of a tape
file does not reveal the Object's name.

Inventory and full verification therefore use the host checkpoint journal, when
one survives. The per-tape checkpoint journal is fsynced (forced to stable
storage) only after each Object body and its trailing filemark are durable.
During the BOT scan, Remanence replays that journal without loading every
Object row into memory. It accepts a name only when all of these facts agree:

- the journal is for the same tape UUID and fixed block size;
- its committed prefix is present in the measured tape layout;
- every journal Object names the same tape-file number and block count that
  the physical scan measured; and
- journal rows and measured Objects match one to one throughout that committed
  prefix.

Each Object is then classified as follows:

| Classification | Meaning |
|---|---|
| `Recovered` | The Object tape file is physically complete, and an exact fsynced checkpoint row supplied its Object identifier. |
| `Unknown` | The Object tape file is physically complete, but no trustworthy local identity source covers it. |
| `Incomplete` | EOD was reached before the Object's trailing filemark. Remanence never applies a checkpoint name to it. |

After a crash, a complete Object can lie physically beyond the last fsynced
checkpoint. It is reported as `Unknown` and is not promoted to `Recovered`. A
corrupt journal, or one whose geometry conflicts with the tape, stops the
operation with a data-loss error and is not ignored.

A tape from another installation normally has no checkpoint journal at the
receiving site, and Remanence does not create one because an inventory was
requested. If all its indexes are invalid, complete Object candidates are
therefore reported as `Unknown`. Transferring the matching checkpoint journal
preserves the exact Object names. Without it, the names come from deeper
REM-OBJECT inspection or from operator recovery. In either case these results
are inspection evidence and do not populate the writable SQLite catalog.

In plain terms, adoption records the native tape's identity, geometry, pool,
and a cautious lifecycle state. It does not rewrite the tape, decrypt or
restore Objects, or invent file and index records.

## When to use it

Typical cases are:

- the rebuildable SQLite catalog was lost, while the cartridges and the durable
  audit or journal material survived;
- an offsite or vault cartridge returned to a replacement host;
- a cartridge is being transferred between Remanence sites;
- a cartridge unknown to the local catalog must be identified before
  terminal-index inventory or a catalog rebuild.

Adoption is not a way to relabel a cartridge, override a retired identity,
turn a foreign format into a Remanence format, or make an already-written tape
writable without recovering its real catalog authority.

For a transfer between sites, the destination must support the tape's
bootstrap and on-tape format version. Its configuration also needs a pool rule
that maps the barcode to an existing pool. Adoption does not decrypt or restore
Objects, and encrypted Objects still need their recovery keys. A WORM
(write-once) cartridge is safe for this read-only identity step but cannot be
used for ordinary Object writes. A cartridge in LTFS, tar, or another foreign
format needs a format-specific adapter and import pipeline, and adoption never
converts it into a Remanence tape.

## Safety model

The workflow reads the tape twice:

1. `bot-probe` is advisory. It resolves one exact barcode in one configured
   library, checks its exact home slot, opens one read-compatible drive, and
   reads BOT with exactly one `LOCATE(0)` (position to the start of the tape)
   and at most one `READ`.
2. `adopt-bootstrap` does not rely on the earlier probe. It takes the
   exclusive `StateHandle` (the lock on the local state directory), performs
   fresh discovery, checks the barcode, library, home slot, and selected drive
   again, and rereads the bootstrap and its immediate physical tail under the
   same drive handle.

Both commands temporarily select fixed 1 MiB blocks with drive compression
disabled. They verify that setting, restore and verify the drive's previous
mode, and park the cartridge in the exact expected home slot. A failure to
restore or park fails the command. Neither command writes tape data,
filemarks, or a new bootstrap.

The adoption read accepts only an RFC 4122 version 4 UUID and the canonical
1 MiB default-parity bootstrap geometry with compression disabled. The
expected UUID argument is compared with the UUID from this fresh BOT read, not
with discovery or catalog data.

`StateHandle` and the daemon use the same local advisory state lock, and the
daemon holds it for its whole lifetime. The lock excludes another cooperating
Remanence daemon or offline state command. It is not a SCSI persistent
reservation, so it cannot exclude unrelated robotics or tape software. Stop the
daemon before importing, and stop every other system that could move or use the
same cartridge or drive.

## Commands

First collect advisory identity evidence:

```text
rem-debug --allow LIBRARY_SERIAL \
  tape bot-probe BARCODE \
  --library LIBRARY_SERIAL \
  --expected-home-slot 0x0401 \
  --json
```

The placeholders are:

- `LIBRARY_SERIAL`: the serial number of the tape library that holds the
  cartridge. `rem libraries` lists the libraries on the host with their
  serials. The same value is given to `--allow` and to `--library`.
- `BARCODE`: the barcode label of the cartridge, as shown by `rem library
  LIBRARY_SERIAL --slots`.
- `0x0401`: an example. Use the element address of the storage slot in which
  the cartridge currently sits, taken from the same `rem library
  LIBRARY_SERIAL --slots` listing. The command returns the cartridge to that
  slot when it finishes.

For a valid native bootstrap, note the reported `tape_uuid` and confirm the
reported library, barcode, source slot, drive, compression flags, and geometry.
Then perform the authoritative adoption. The command generates its own
operation UUID, so the caller does not need to create or keep an operation
receipt.

```text
rem-debug --allow LIBRARY_SERIAL \
  tape adopt-bootstrap BARCODE \
  --library LIBRARY_SERIAL \
  --expected-home-slot 0x0401 \
  --expected-existing-tape-uuid TAPE_UUID_FROM_PROBE \
  --json
```

`LIBRARY_SERIAL`, `BARCODE`, and the home slot are the same values as for the
probe. `TAPE_UUID_FROM_PROBE` is the `tape_uuid` field of the probe's JSON
output. Adoption compares it with the UUID in its own fresh read of BOT, and
refuses if they differ.

The probe emits schema `rem.tape.bot-probe.v1`. Its important fields are
`physical_disposition`, `library_serial`, `library_revision`, `barcode`,
`source_slot`, `drive_element`, `drive_serial`, `configured_drive_compression`,
`bootstrap_drive_compression`, `tape_uuid`, `geometry`,
`canonical_adoption_geometry`, and `detail`.

Adoption emits schema `rem.tape.adopt-bootstrap.v1`. In addition to physical
identity and geometry it reports `physical_tail`, `newly_adopted`,
`operation_id`, `idempotency_fingerprint`, and the resulting identity-only
`catalog` projection, which includes the state, pool, and assignment
generation.

## What adoption creates

A successful first adoption fsyncs a `TapeIdentityAdopted` audit record before
it projects the SQLite row. The row contains the exact tape UUID, barcode,
canonical geometry, configuration-derived pool, lifecycle state, and assignment
generation. It creates no tape-file, Object-copy, catalog-unit, wrap-map,
committed-prefix, session, fence, or finalization authority.

The lifecycle state is deliberately conservative:

- exactly `Bootstrap → one filemark → EOD` becomes `ready`. This is an
  identity-only bootstrap tape with no later physical content;
- data after the bootstrap, a missing or extra filemark, or an unreadable tail
  after a valid bootstrap becomes `recovery_required`.

`recovery_required` does not mean the identity is damaged. It means the
identity was recovered, but the tape has more, different, or uncertain physical
content that must be inventoried before normal use.

## Crash recovery and retries

### Interrupted Object writes are retried whole

An Object is one complete REM-OBJECT tar stream, possibly holding many
separately named and checksummed files. Remanence writes the filemark only after
all of the Object's blocks have been written, so a power cut never leaves “part
one, filemark, part two”. Members inside a bundle have no tape filemarks between
them.

If the host stops while writing the body, the tail may hold some physical
blocks but has no committed Object delimiter and no catalog entry. On an open,
parity-off, rewritable tape, the last durable checkpoint journal records the
EOD after the previous complete Object. After a restart, a new write session
positions at that checkpoint EOD, and the caller sends the interrupted Object
again from byte zero. The rewrite replaces the uncommitted tail, and only the
completed retry gets a filemark and enters the catalog. A stock tar
implementation therefore still sees one complete tar stream for the Object, not
two pieces that need Remanence-specific joining.

This cannot be done in place on WORM media, where the incomplete tail cannot be
erased. Remanence admits ordinary Object sessions only when the drive reports
`NotWorm`; `Worm` and `Unknown` are both refused before the drive mode is
changed. Read-only adoption and recovery are safe on WORM, and terminal-index
finalization has its own append-only recovery path for WORM. Remanence does not
implement WORM Object ingest.

The caller keeps the source until `CheckpointSession` reports the Object
committed. Resuming a session is not the recovery method. An interrupted
session is abandoned, and every Object that was not checkpointed is replayed in
full with the same caller id, input kind, content digest, archived logical-file
path, and copy intent. Copy intent is plaintext versus encrypted and, for
encrypted copies, the exact ordered recipient epoch ids. If the catalog already
holds that exact request, the retry reads back the matching committed copy
(an idempotent retry). Changing any of these fields under the same caller id is
refused. For a canonical REM-OBJECT, the bytes already bind every member path,
so the outer `archive_path` is not part of the replay identity.

The canonical append surface creates an Object and its first copy as one
checkpointed operation. Media completion can come before the checkpoint and
catalog commit, which is why an interrupted, uncommitted tail can exist and must
be rewritten whole. The surface does not attach another copy to an Object UUID
that is already cataloged outside the exact pool and caller replay key; that
conflict is refused before any tape motion. A daemon-wide claim holds the pool
and caller key and the canonical Object UUID from admission, before any motion,
through the checkpoint. Two drive actors therefore cannot both write the same
identity.

The one-shot direct writer follows the same rule under the exclusive state
lock. Before it selects a tape, it replays every checkpoint journal in the
configured state directory, not only the journal of the tape it might select,
and resolves an exact committed retry from host state alone. A committed retry
therefore needs no writable tape and moves no cartridge. This also covers the
case where tape A has a durable checkpoint but SQLite has not yet recorded the
Object identity: a retry cannot select tape B and write the same identity again.
The drive-bound writer repeats the reconciliation and replay check. It takes
the pool policy, checkpoint directory, parity-journal path, and memory ceiling
from the locked operator configuration, not from its caller. Host-only
encrypted replay reports the committed ordered epoch ids. The host catalog does
not keep the authenticated on-tape labels, so those labels are reported as
unknown instead of being copied from the new retry request.

If a checkpoint append reports an error and the rollback also fails, Remanence
cannot know whether the new frame is absent, torn, or complete. This is the
distinct `checkpoint append authority uncertain` state. The affected Object
identities stay quarantined for the life of the process and the write fails. A
later process must reconcile the journal before it admits a retry. A complete
valid frame is projected into the catalog, and a torn trailing frame is not
treated as committed.

This mechanism depends on the host checkpoint journal of an open tape. A
cartridge arriving from another installation does not gain safe append
authority just because its bootstrap can be adopted. Transfer the matching host
journals when they are available. Otherwise keep the cartridge out of ordinary
write service and use terminal inventory or the bounded BOT recovery to find
out what is on it. Adoption alone never guesses the last committed append
position.

A drive or transport failure can leave the completion of the last command
uncertain. Remanence then fences that tape and does not assume how much was
written. The whole-Object rule still applies, but an operator must first
reconcile or release the fence using the recorded evidence. The end-to-end
recovery drill on a virtual tape library exercises an abrupt daemon loss after a
completed write-command boundary. It does not emulate the volatile-cache
behavior of any particular physical drive.

### Finalization recovery

When a tape is finalized, the writer closes the open parity epoch and writes
the three terminal replicas, A, B, and C, with a separation extent (a control file
with a header, zero-filled interior records, and a footer) between each pair. If the checkpoint and parity journals prove that
replica C completed its synchronizing barrier, all three full indexes are
already on tape. A restart then completes the remaining bookkeeping without
loading, locating, reading, or writing the cartridge. That bookkeeping is the
sealed checkpoint, the SQLite projection, cleanup of the intent record, and the
audit entries. It applies to automatic finalization and to an operator-requested
close-out.

During finalization Remanence keeps a small durable record on the host, which
the code calls the **companion intent**. It records what the writer was doing
and where a restart must resume. It is not an index or checkpoint on the tape.

If the operation was already classified `recovery_required`, the companion
keeps that flag until the sealed checkpoint is safely on disk, so a failure
before that point cannot silently downgrade the tape to ordinary `finalizing`.
After the sealed checkpoint is durable, recovery can replay it and remove the
stale companion. If a sealed checkpoint and a companion exist together after a
crash, recovery first proves that they describe the same completion. It then
repairs SQLite while keeping the companion, and removes the companion only
after the repair succeeds. Changes to a pool's capacity cap or watermarks do not
block this host-only bookkeeping, because the complete reserved tail is already
on the tape.

An ordinary write session cannot take ownership while the companion exists,
even if the sealed checkpoint already matches it. The explicit terminal-recovery
path must record the completion first and remove the companion second. This
keeps a normal append from discarding the information a later restart needs to
finish recovery.

The same restart repairs missing completion records in the append-only audit
log. It records one tape-sealed event and, for a manual close-out, one
operation-finished event. An event written before the crash is reused and not
duplicated. Both records are always flushed to stable storage. Only one daemon or
offline state owner can use a state directory at a time, so another process
cannot race that decision.

Before the replica C boundary, a failed or uncertain component is recorded
durably as `recovery_required`. A restart does not turn that state back into
ordinary `finalizing`. It reconciles the next physical component first. A host
metadata failure after a proved replica C therefore causes no needless tape
motion, while genuinely uncertain media stays fenced.

### Adoption retries

If the process stops after the audit append but before the SQLite projection,
the next invocation replays the audit record and repairs the missing
projection. One operation UUID cannot be rebound to a different tape or to
changed facts.

Each retry by the caller gets a newly generated operation UUID. After the audit
replay, the command returns an `already adopted` no-op only when the current
row is still the same identity-only row: the same UUID, barcode, geometry, pool,
assignment generation, and tail-derived state, with no Object, file, prefix, or
finalization authority, no open session, no active fence, and no active media
authority. No second adoption audit record is needed. Once writes or lifecycle
changes have evolved the row, re-adoption is refused and does not roll the state
back.

## After adoption

For a tape reported `ready` because it holds exactly a bootstrap, the catalog
has enough identity to treat it as an empty native cartridge. It still has no
invented Object history.

For `recovery_required`, keep the tape out of write service. Start the daemon
under controlled ownership and inspect the terminal indexes through its API:

```text
rem tape inventory --tape-uuid TAPE_UUID --json
rem tape verify-index --tape-uuid TAPE_UUID --json
```

`TAPE_UUID` is the `tape_uuid` reported by the probe and the adoption. Both
commands accept the same arguments, and neither has a depth option.

**Inventory** is the quick check. It reads the bootstrap at BOT and the terminal
replicas at the end of the tape. When a terminal replica is usable, it reads
that replica's payload (normally replica C's), reads no Object data, and does
not measure the tape's whole prefix, so it does not prove that the middle of
the tape matches what the replicas describe. It tries the terminal replicas in
the defined fallback order. If none survives, it falls back to BOT recovery,
which is reported explicitly, walks the tape from BOT, measures the prefix, and
reads selected Object blocks.

**Full verification** (`verify-index`) is a separate operation. It reads every
data block that a sidecar protects and every parity shard, checks each against
its sidecar's index, and reports each failure by its address. It also measures
and validates the physical prefix and the terminal structures. A check of
structure and metadata alone, which reads no data block or parity shard, is not
a full verification (REM-PARITY Section 2.2). On a tape written with parity, full verification reads every
protected data block and parity shard, so it takes at least those bytes divided by
the drive's sustained read rate. For a full 18 TB LTO-9 cartridge at the half-height
drive's native rate of 300 MB/s, that is 18 × 10^12 bytes ÷ 300 × 10^6 bytes
per second = 60,000 seconds, or 16 hours 40 minutes. Repositioning adds to this.
[Full verification](guide-full-verification.md) gives the formula for the
additional time and explains when the same check is useful as a periodic audit
of the whole archive.

An inventory that finds all indexes invalid can run for hours, so its stream
announces `bot_recovery_started` before the full BOT walk. It then emits one
`bot_recovery_progress` event after every complete tape file, with the
tape-file number, the partition and LBA position, the candidates found so far,
and the elapsed milliseconds. Human-readable output shows the same facts as
`bot_recovery:` and `bot_recovery_progress:` lines. Cancellation is checked at
each of these boundaries and stops before the next tape file is read. These are
additional event types in `rem.tape.inventory.stream.v1`. Use a daemon and CLI
from the same build to get the BOT progress events.

The walk can start even on a tape whose three terminal replicas are intact. A
tape file written after the terminal suffix, or a second terminal suffix, moves
EOD past the EOD planned by the layout, so that layout is not used. The
exception is a later suffix whose own replicas validate: its footer supplies its
own layout, and no walk is needed (REM-PARITY Section 8.4). [Damaged
tapes](guide-damaged-tapes.md) explains why a tape with intact replicas can
still be walked and how to read what the walk reports.

Stop the daemon again before changing host state. Restore any surviving
per-tape journals and audit material to their configured locations, then
rebuild the SQLite projection offline when appropriate:

```text
rem rebuild-catalog-from-journals --config /path/to/config.toml
```

`/path/to/config.toml` is the daemon's configuration file, by default
`/etc/rem/config.toml`.

Streamed inventory rows are not committed catalog entries unless the inventory
summary and the subsequent recovery or rebuild procedure say they are.

A successful adoption means “we now know exactly which tape this is”. It does
not mean “this tape is ready for new writes”. A `recovery_required` tape stays
out of normal write use until the inventory, the commit history, and the local
catalog agree.

## Refusals to expect

The commands refuse to proceed for any of these reasons: an ambiguous barcode,
a wrong library or home slot, a UUID mismatch, a damaged or non-v4 bootstrap
identity, a foreign or unrecognized format, noncanonical geometry, enabled
compression, active media-readiness ownership, a conflict between barcode and
pool, a conflict with any tape kind, a retired identity, or an existing row
that carries file, Object, prefix, finalization, open-session, active-fence, or
other evolved authority. Failures of physical readiness, mode restoration, and
exact parking also prevent any catalog change.

## Practical import checklist

1. Stop the daemon, robotics, backup software, and any other drive owner.
2. Confirm the target library is configured and the cartridge barcode appears
   exactly once.
3. Record the exact storage (home) slot element address and make sure the
   destination is usable.
4. Run `bot-probe`. Require `bootstrap_valid`, canonical geometry, compression
   off, and the expected library, barcode, and home-slot evidence.
5. Take the UUID from the probe's output, not from a stale catalog.
6. Run `adopt-bootstrap` with that UUID and the same library and home slot, and
   review the internally generated operation UUID in the JSON result.
7. Confirm the cartridge was parked and inspect the returned catalog state.
8. If the state is `recovery_required`, start the daemon under exclusive
   controlled ownership, keep the tape out of write service, and run terminal
   inventory and verification.
9. Stop the daemon again before restoring transferred journals or running the
   offline catalog rebuild. If no host journals survived, do not rebuild from
   the inventory. Keep the tape read-only, because Remanence does not import
   tape-only inventory rows into the catalog.
10. Resume the daemon or normal site operations only after ownership and the
    recovered catalog authority are reconciled.
