# Full verification

This page explains what `rem tape verify-index` checks, what it means when the
command reports a tape as complete, what it prints when it finds a problem,
and how long it takes. It is written for the person who has to decide whether a
tape is good. The command's row in the [CLI reference](reference-cli.md) states
what it does in a line; this page gives the reasons. The rule it follows is
REM-PARITY Section 2.2, in the preparing copy of the specification,
[REM-PARITY 1.0.0-draft.5](../specs/in-progress/rem-parity-1-specification.md).
For what to do when a tape does not verify, or will not identify itself, see
[Damaged tapes](guide-damaged-tapes.md).

<!-- code-anchor: crates/remanence-parity/src/verify_protected.rs crates/remanence-api/src/write_owner/terminal_inventory.rs @ c80a553a -->
## What a full verification is

A tape can be checked at two depths. The shallow check reads the bootstrap,
the terminal index, and the structure of the files in between: where each tape
file starts and ends, whether the three terminal replicas agree, and whether
the tape ends where the index says it should. It reads none of the data that
Objects are made of. A tape can pass it and still hold a block that no drive
can read.

The deep check is a full verification. REM-PARITY Section 2.2 defines it in
these words: "A Verifier's validation is a full verification: it reads every
data block that a sidecar protects and every parity shard, and checks each
against its sidecar's index (Section 13.4)." The same paragraph says the other
side of the line just as plainly: "A check of structure and metadata alone,
which reads no data block or parity shard, is not a full verification." An
older note, or an older version of this documentation, may have called the
shallow check a full verification. It is not one, and `rem tape verify-index`
performs the deep check.

The reason for the definition is what a parity tape promises. Each Object's
blocks are protected by parity shards written into sidecar tape files. The tape
can only be called good if those blocks and shards can still be read and still
match the checksums the sidecar recorded for them. Only a read of every block
and shard can show that, and a full verification does that read.

A full verification treats each data block as opaque bytes. It does not open
an Object, does not interpret its contents, and does not decrypt anything. It
also does not repair anything: it reads and reports.

## Running it

The command takes the tape's UUID and runs through the daemon, like
`rem tape inventory`:

```text
rem tape verify-index --tape-uuid TAPE_UUID
rem tape verify-index --tape-uuid TAPE_UUID --json
```

The drive is occupied for the whole run. Plan for that before you start; the
section [How long a full verification takes](#how-long-a-full-verification-takes)
gives the arithmetic.

## When a tape is verified complete

A tape is verified complete when four things hold together.

1. The terminal suffix is complete. All three replicas of the terminal index
   validate in full, both separation extents validate, and the end of data
   follows the trailing filemark of the last replica, C (REM-PARITY Section
   12.6).
2. The walk of the prefix, which is everything before the terminal suffix,
   finds nothing wrong.
3. Every data block that a sidecar protects reads and matches its sidecar's
   index, and every parity shard reads and matches.
4. There is no finding about a sidecar copy or footer, about a copy or footer
   of the ParityMap, or about the bootstrap's trailing fill.

A failure of any one of these leaves the tape not complete. The specification
states the same condition in one sentence: the Verifier "reports the tape as
complete when the terminal suffix is complete, the full verification was
performed, and it finds no failed data block, no failed parity shard and no
finding about a sidecar or about the prefix", and a finding about a copy of
the ParityMap or its footer likewise leaves the tape not complete.

The fourth condition includes the fill at the end of the bootstrap block. The
bootstrap is one block, and the bytes after its payload are written as zeros.
REM-PARITY Section 8.1 tells a Reader never to let that fill decide whether it
accepts the bootstrap, so that a damaged byte cannot cost the tape its entry
point. It tells a Verifier to check it and to report a nonzero fill as a
nonconformity, so a tape whose bootstrap fill is not zero is readable and is
not complete. The Verifier reports the fill only when it can read the
bootstrap.

## What the command prints

`rem tape verify-index` prints two lines that are easy to mistake for one
another, `verified` and `complete`. They describe different things. `verified:
true` describes the run: the verification ran to the end and reached a
verdict. `complete` describes the tape: whether it passed. In this command the
two agree with the state line, and `complete` is true only when
`verification_state` is `verified_complete`. A tape that failed prints
`verification_state: verified_degraded` and `complete: false`.

The lines of a run that reached a verdict come in this order:

```text
verification_state: verified_degraded
verified: true
complete: false
verification_basis: measured_full_physical
protected_content_finding: KIND [ADDRESS] DETAIL
measured_eod_lba: ...
verified_prefix_tape_file_count: ...
verified_prefix_record_count: ...
measured_tape_file_count: ...
edition_digest: ...
layout_digest: ...
payload_digest: ...
canonical_map_digest: ...
replica_health:
  A: STATE (DETAIL)
  B: STATE (DETAIL)
  C: STATE (DETAIL)
separation_health:
  AB: STATE (N interior records; DETAIL)
  BC: STATE (N interior records; DETAIL)
verification_detail: ...
```

The `protected_content_finding` line appears once for each finding, and not at
all on a tape with none. The state line and the finding lines are what decide
the outcome; the rest describe the terminal suffix. The example above shows
the shape of each line and not the output of a particular tape.

A third state exists. When the command cannot verify the prefix against a
terminal index, it prints `verification_state: recovery_required` and
`verified: false`, then `measured_eod_lba`, `measured_tape_file_count` and
`verification_detail`, and then the inventory that a structural walk from the
beginning of the tape recovered. The state has three causes: no replica of the
terminal index validates; the tape's end of data lies beyond the end the layout
planned, because something was written after C; or the prefix is truncated
before the planned prefix. The first two are described in
[Damaged tapes](guide-damaged-tapes.md). This state prints no `complete` line
and no `verification_basis` line in the human form. `verification_basis:
bot_structural_recovery` appears only in the JSON, and so do any findings from
the pass over the protected content, which still runs over the walked map; the
human form does not print them. Such a tape is not verified against a terminal
index.

`verified_degraded` also has a second cause that involves no finding line. A
replica or a separation extent can be damaged while the prefix and the
protected content are intact. In that case there are no
`protected_content_finding` lines, `replica_health` or `separation_health`
names the damaged component, and the tape is not complete because its terminal
suffix is not.

## The six kinds of finding

The text after `protected_content_finding:` is the kind of the finding, printed
in full, then an address in square brackets, then a detail. The kind is one of
six names, and the address says where to look.

| Kind | What was found | Address | Remarks |
| --- | --- | --- | --- |
| `PROTECTED_CONTENT_FINDING_KIND_DATA_BLOCK` | A data block that a sidecar protects failed. | A tape-file position, such as `tape_file 1 block 2`. | The detail is one of: `unreadable`, `record of the wrong length`, `filemark or end of data where a block belongs`, or `CRC mismatch`. The last means the block read and its checksum disagrees with the sidecar's index. |
| `PROTECTED_CONTENT_FINDING_KIND_PARITY_SHARD` | A parity shard failed. | The epoch, stripe and parity index, such as `epoch 0 stripe 1 parity 0`. | The same four details as for a data block. |
| `PROTECTED_CONTENT_FINDING_KIND_SIDECAR` | A sidecar's metadata has a finding: a copy, its footer, or its index. | The sidecar's tape file and epoch, such as `tape_file 2 epoch 0`. | The detail begins with a name from REM-PARITY Section 15 where one applies (`SidecarParse`, `SidecarMetadataUnavailable`, `SchemeMismatch`), or a copy-health finding, whose detail begins `(copy health; no Section 15 name):` and goes on, for example, `footer could not be read (medium error)` or `primary copy was not located by any locator`. |
| `PROTECTED_CONTENT_FINDING_KIND_PARITY_MAP` | A copy of the ParityMap, or its footer, was unreadable or invalid. | The ParityMap's tape-file number, such as `tape_file 7`. | Reported even when the other copy was used and the ParityMap as a whole is usable. The tape is still not complete. |
| `PROTECTED_CONTENT_FINDING_KIND_PREFIX_DAMAGE` | Damage or nonconformity inside the prefix, including nonzero trailing fill in the bootstrap. | None; the brackets are empty. | For a fill finding the detail names the bootstrap's tape file (`bootstrap at tape_file N has nonzero trailing fill (REM-PARITY 8.1)`). For damage the walk found, it gives the kind of damage and its position, such as `UnreadableTapeFileHead at LBA 0`. |
| `PROTECTED_CONTENT_FINDING_KIND_NOT_PERFORMED` | The pass over the protected content could not be carried out. | None; the brackets are empty. | The detail gives the reason. The tape is not known to be good, and it is not reported as complete. |

The findings are printed in a fixed order: the pass not performed, then
prefix damage, ParityMap findings, sidecar findings, failed data blocks, and
failed parity shards. With `--json`, the same findings are carried in a
`protected_content_findings` array with one object per finding, holding the
kind, the address (or null) and the detail, so that a script can say which
block failed and does not have to read a count out of the `detail` string.

A finding does not mean the tape is lost, and this page does not name a repair
command, because none of the commands in the [CLI reference](reference-cli.md)
acts on a finding. The finding says where the damage is. Whether an Object that
touches a failed block can still be read depends on the parity: a stripe that
has lost no more blocks and shards than the scheme's `m` can be reconstructed
by a Reader that recovers from parity (REM-PARITY Sections 13.4 and 13.5). A
stripe that has lost more cannot. This documentation does not name an operator
command that performs that read.

## An epoch with no usable index

A sidecar's index is what a full verification checks each block and shard
against. When no copy of an epoch's index validates, there is nothing to check
against for that epoch. The Verifier still reads every data block that the
ParityMap says the sidecar protects and every parity shard it locates, reports
each read failure, and states that what it read could not be checked against a
checksum. It does not call a block failed for lack of one. The other epochs are
unaffected. The tape is not complete, because the epoch's sidecar has a
finding. The Verifier's rule for this case is in REM-PARITY Section 2.2, and
the way an implementation acquires the index is described in the
[implementation guide](../specs/publication/rem-implementation-guide.md).

## A tape written without parity

A tape written without parity has no sidecars, so no data block is protected
by one, and a full verification has nothing further to read. It still checks
the terminal suffix, the prefix, and the bootstrap's trailing fill. A no-parity
tape that verifies complete has therefore shown that its structure is sound,
and it has not shown that every data block reads. The checksums that could show
that were never written.

<!-- code-anchor: crates/remanence-parity/src/verify_protected.rs @ c80a553a -->
## How long a full verification takes

A full verification reads every data block and parity shard on the tape. Its
duration is therefore at least the number of bytes on the tape divided by the
sustained read rate of the drive. This is arithmetic. It is not a measurement
of a full verification, because none has been run on a full cartridge.

A full LTO-9 cartridge holds 18 TB natively, which is 18 x 10^12 bytes, about
16.4 TiB. Two read rates give two figures.

- At 200 MiB/s, which is 209,715,200 bytes per second: 18 x 10^12 divided by
  209,715,200 is about 85,800 seconds, or 23.8 hours. The figure of 200 MiB/s
  is a streaming read rate measured on physical LTO-9 hardware during testing.
  It is not the measured rate of a full verification. A slower rate takes
  longer.
- At the LTO-9 native rate of 400 MB/s, which is 400 x 10^6 bytes per second:
  18 x 10^12 divided by 400 x 10^6 is 45,000 seconds, or 12.5 hours. A
  half-height drive may be rated below this rate, and then it takes longer.
  Use the rating of your own drive.

The figures rest on assumptions, and they are a starting point for a plan and
not a promise. Twelve and a half hours is a lower bound; 23.8 hours is what
the measured streaming rate gives, and it is not an upper bound. The
assumptions are these.

- The drive streams without stopping. The verification pass reads in order and
  repositions the drive only when the next block is not where the previous read
  left it, but it must reposition between tape files and to each sidecar, and
  those repositions are not counted.
- The host, the SCSI adapter and the verification pipeline keep up with the
  drive at the stated rate.
- No read errors force retries.
- The bytes read are the bytes written, parity included. A parity tape holds
  `k + m` shards for every `k` data blocks, so the bytes read exceed the user's
  bytes. A tape that is one third full takes about one third as long.
- Loading the cartridge, positioning to the beginning of tape, and reading the
  terminal suffix add time and are not counted. So does the walk of the prefix
  that comes first, which spaces over one filemark for each tape file. Each
  sidecar's header and index copies are also read twice, once when the copy is
  checked and once when the index is acquired.

The [implementation guide](../specs/publication/rem-implementation-guide.md) says of the walk from
the beginning of the tape that it "can take hours". A full verification is of
the same order or longer: on the scale of a working day for a full LTO-9
cartridge, with the drive occupied throughout. Run it in a maintenance window.
Do not run it to check a tape just before a read that will read the same
blocks anyway.

This page gives no duration for a check of structure alone. The time of a walk
grows with the number of tape files, and on a tape of many small Objects the
repositioning alone can take hours. No figure has been measured.

## In plain terms

Verifying a tape completely means reading every page of a very long book, at
the speed the drive turns the pages. A full LTO-9 cartridge is roughly half a
day to a day of reading. A look at the cover and the table of contents is much
quicker, but it cannot tell you that the pages are readable, which is why the
tool no longer calls that a full verification. "Verified" says the reading
finished, "complete" says every page was good, and when a page was not, the
tool tells you which one.
