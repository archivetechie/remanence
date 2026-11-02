# Full verification

This guide is for an operator who wants to know whether a tape in a library is
still good, or who plans regular checks of an archive. A full verification
reads every data block and every parity shard that the tape's parity sidecars
protect, and checks each against the checksum recorded for it. It has two
practical uses. One is a periodic check of the whole archive, for example
verifying every tape in turn on a rotation, so that unreadable blocks are found
early enough to re-copy the data. The other is to judge one suspect tape.

A tape written without parity has no sidecars, so for it the verification
checks structure only (see [A tape written without
parity](#a-tape-written-without-parity)). For an epoch whose sidecar index
cannot be read, it reads the blocks but has no checksum to compare them
against.

Several terms from the tape layout recur on this page; the
[glossary](reference-glossary.md) has each of them. A **parity sidecar**, or
sidecar, is a tape file of parity shards (blocks of parity data) and per-block
checksums that covers the data blocks of one epoch. An epoch is one group of
data blocks that are protected together and described by one sidecar; at the
default settings a full epoch covers 16 GiB of object data. The **ParityMap**
is the tape file written after the last sidecar; it lists every sidecar and the
range of data each protects.

The **prefix** is everything on the tape before its terminal suffix: the
bootstrap, the objects, the sidecars and the ParityMap. The **terminal suffix**
is the five tape files at the end: the three copies (replicas A, B and C) of
the terminal index, which lists the tape's objects, and the two **separation
extents**, AB and BC, spacer tape files that keep the replicas physically
apart. The specification calls the program that checks a tape the Verifier;
`rem tape verify-index` plays that role.

This page explains what `rem tape verify-index` checks, what it means when the
command reports a tape as complete, what it prints when it finds a problem,
and how long it takes. The rule it follows is REM-PARITY Section 2.2, in the
draft specification under review,
[REM-PARITY 1.0.0-draft.5](../specs/in-progress/rem-parity-1-specification.md).
For what to do when a tape does not verify, or will not identify itself, see
[Damaged tapes](guide-damaged-tapes.md).

<!-- code-anchor: crates/remanence-parity/src/verify_protected.rs crates/remanence-api/src/write_owner/terminal_inventory.rs crates/remanence-cli/src/tape_inventory.rs @ 625f7044 -->
## What a full verification is

REM-PARITY Section 2.2 defines it in these words: "A Verifier's validation is
a full verification: it reads every data block that a sidecar protects and
every parity shard, and checks each against its sidecar's index (Section
13.4)." The same paragraph says where the line falls: "A check of structure
and metadata alone, which reads no data block or parity shard, is not a full
verification." `rem tape verify-index` performs the full verification, and it
has no shallower mode.

The quick check is a different command, `rem tape inventory`. It reads the
bootstrap at the beginning of the tape and the terminal index replicas at the
end, and normally reads nothing in between. It reads no object data and
normally takes minutes. When no replica can be used, it falls back to a
structural walk from the beginning of the tape, which reads the first and last
block of each object and can take hours. A successful inventory tells you that
the tape's index, or the structure the walk recovered, is readable and
consistent. It does not tell you that the data blocks can be read: a tape can
pass an inventory and still hold a block that no drive can read.

The two commands therefore fit together. Run `rem tape inventory` as the quick,
frequent check, and a full verification on a rotation whose interval suits the
number of tapes and drives you have.

The reason for the definition is the guarantee a parity tape makes. Each
object's blocks are protected by parity shards in sidecar tape files. The tape
can only be called good if those blocks and shards can still be read and still
match the checksums the sidecar recorded for them, and only a read of every
block and shard can show that.

A full verification treats each data block as opaque bytes. It does not open an
object, does not interpret its contents and does not decrypt anything. It also
repairs nothing: it reads and reports.

## Running it

The command takes the tape's UUID, which `rem catalog tapes` prints, and runs
through the daemon, like `rem tape inventory`:

```sh
rem tape verify-index --tape-uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f
rem tape verify-index --tape-uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f --json
```

It refuses a barcode. A run that waits for the verdict exits with status 0 only
when the tape is verified complete, and with status 1 for every other verdict,
so a script can test the exit status alone. The drive is occupied for the whole
run; [How long a full verification takes](#how-long-a-full-verification-takes)
gives the arithmetic.

To start a verification without waiting for it, add `--detach`. The command
prints `operation <UUID>` and returns. `rem op watch <UUID>` follows its
progress, and `rem tape verify-index --result <UUID>` prints the verdict once
it is ready, in the same form as an attached run.

If the cartridge was just loaded and the drive is not yet ready, the daemon
refuses with a media-readiness fence and starts waiting for the drive itself.
An attached `verify-index` then waits as well, for up to 15 minutes by default,
and prints on standard error:

```text
verification blocked by media readiness operation <UUID>; waiting up to 900s (pass --ready-wait 0 to fail fast)
```

It prints each change of the readiness state, and retries the verification once
the drive is ready. `--ready-wait <DURATION>` changes how long the command
waits (in `ms`, `s`, `m` or `h`), and `--ready-wait 0` returns the refusal at
once. The daemon keeps waiting for the drive after the command gives up.
[Media-readiness fences and
quarantine](guide-troubleshooting.md#media-readiness-fences-and-quarantine)
describes those waits and how to release a fence that does not clear.

## When a tape is verified complete

A tape is verified complete when four conditions hold.

1. The terminal suffix is complete. All three replicas of the terminal index
   validate in full, both separation extents validate, and the end of data
   follows the trailing filemark of the last replica, C (REM-PARITY Section
   12.6).
2. The walk of the prefix finds nothing wrong.
3. Every data block that a sidecar protects reads and matches its sidecar's
   index, and every parity shard reads and matches.
4. There is no finding about a sidecar copy or footer, about a copy or footer
   of the ParityMap, or about the bootstrap's trailing fill.

A failure of any one of these leaves the tape not complete. The specification
states the same condition in one sentence: the Verifier "reports the tape as
complete when the terminal suffix is complete, the full verification was
performed, and it finds no failed data block, no failed parity shard and no
finding about a sidecar or about the prefix". A finding about a copy of the
ParityMap or its footer also leaves the tape not complete.

The fourth condition includes the fill at the end of the bootstrap block. The
bootstrap is one block, and the bytes after its payload are written as zeros.
REM-PARITY Section 8.1 requires a Reader to accept the bootstrap whatever the
fill contains, so a damaged byte in the fill cannot make the tape unreadable.
It also tells a Verifier to check the fill and to report a nonzero fill as a
nonconformity. A tape whose bootstrap fill is not zero is therefore readable,
and not complete. The fill is reported only when the bootstrap can be read.

<!-- code-anchor: crates/remanence-cli/src/tape_inventory.rs @ 625f7044 -->
## What the command prints

`rem tape verify-index` prints two lines that are easy to mistake for one
another, `verified` and `complete`. They describe different things. `verified:
true` describes the run: the verification ran to the end and reached a verdict.
`complete` describes the tape: whether it passed. `complete` is true only when
the state line says `verified_complete`. A tape that failed prints
`verification_state: verified_degraded` and `complete: false`.

The human output starts with the `io` lines described in
[The I/O report](#the-io-report), then gives the verdict. The verdict of a run
that reached one has this shape, with one line for each field:

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
all on a tape with none. The state line and the finding lines decide the
outcome. The counts describe what was measured: `measured_eod_lba` is the
logical block address (LBA), the drive's number for a record's position, at
which the end of data was found. `verified_prefix_tape_file_count` counts the
tape files of the prefix, and `measured_tape_file_count` every tape file from
the beginning of the tape through replica C, so it includes the five files of
the terminal suffix. The four digests identify what was verified: the
**edition** (the facts that replicas A, B and C share, such as the number of
tape files they cover), the planned layout of the terminal suffix, the index's
payload and the canonical filemark map. The daemon records the verification, and
`rem catalog tapes --json` later shows the verified edition digest as
`last_verified_edition_digest`.

For a tape with one damaged data block, the verdict looks like this (trimmed;
the values are illustrative):

```text
verification_state: verified_degraded
verified: true
complete: false
verification_basis: measured_full_physical
protected_content_finding: PROTECTED_CONTENT_FINDING_KIND_DATA_BLOCK [tape_file 3 block 1187] CRC mismatch
measured_eod_lba: 52496
verified_prefix_tape_file_count: 12
verified_prefix_record_count: 43521
measured_tape_file_count: 17
...
replica_health:
  A: complete (payload_valid)
  B: complete (payload_valid)
  C: complete (payload_valid)
separation_health:
  AB: valid (4094 interior records; header_footer_zero_fill_and_filemark_valid)
  BC: valid (4094 interior records; header_footer_zero_fill_and_filemark_valid)
...
```

The terminal suffix is intact, and block 1187 of tape file 3 read but did not
match its checksum. A replica that validated only its envelope, without its
payload being read, shows `envelope_valid (envelope_valid_payload_not_read)`; a
failed one shows `invalid` with the reason.

`verified_degraded` also has a second cause that involves no finding line. A
replica or a separation extent can be damaged while the prefix and the
protected content are intact. Then there are no `protected_content_finding`
lines, `replica_health` or `separation_health` names the damaged component, and
the tape is not complete because its terminal suffix is not.

A third state exists. When the command cannot verify the prefix against a
terminal index, it prints `verification_state: recovery_required` and
`verified: false`, then `measured_eod_lba`, `measured_tape_file_count` and
`verification_detail`, and then the inventory that a structural walk from the
beginning of the tape recovered. The state has three causes: no replica of the
terminal index validates; the tape's end of data lies beyond the end the layout
planned, because something was written after replica C; or the prefix ends
before the planned prefix does. [Damaged tapes](guide-damaged-tapes.md)
describes the first two. In the human form, this state prints no `complete`
line and no `verification_basis` line; `verification_basis:
bot_structural_recovery` appears only in the JSON. The pass over the protected
content still runs over the walked map, and its findings also appear only in
the JSON. Such a tape is not verified against a terminal index.

With `--json`, the response uses schema `rem.tape.verify-index.v1`. It carries
the same facts, with the detail line as `detail`, the replica and separation
health as arrays of objects, and every count as a decimal string.

## The six kinds of finding

The text after `protected_content_finding:` is the kind of the finding, printed
in full, then an address in square brackets, then a detail. The kind is one of
six names, and the address says where to look.

| Kind | What was found | Address | Remarks |
| --- | --- | --- | --- |
| `PROTECTED_CONTENT_FINDING_KIND_DATA_BLOCK` | A data block that a sidecar protects failed. | A tape-file position, such as `tape_file 1 block 2`. | The detail is one of: `unreadable`, `record of the wrong length`, `filemark or end of data where a block belongs`, or `CRC mismatch`. The last means that the block read and its checksum disagrees with the sidecar's index. |
| `PROTECTED_CONTENT_FINDING_KIND_PARITY_SHARD` | A parity shard failed. | The epoch, stripe and parity index, such as `epoch 0 stripe 1 parity 0`. | The same four details as for a data block. |
| `PROTECTED_CONTENT_FINDING_KIND_SIDECAR` | A sidecar's metadata has a finding: a copy, its footer, or its index. | The sidecar's tape file and epoch, such as `tape_file 2 epoch 0`. | The detail begins with a name from REM-PARITY Section 15 where one applies (`SidecarParse`, `SidecarMetadataUnavailable`, `SchemeMismatch`), or with `(copy health; no Section 15 name):` for a finding about one copy, followed by text such as `footer could not be read (medium error)` or `primary copy was not located by any locator`. |
| `PROTECTED_CONTENT_FINDING_KIND_PARITY_MAP` | A copy of the ParityMap, or its footer, was unreadable or invalid. | The ParityMap's tape-file number, such as `tape_file 7`. | Reported even when the other copy was used and the ParityMap as a whole is usable. The tape is still not complete. |
| `PROTECTED_CONTENT_FINDING_KIND_PREFIX_DAMAGE` | Damage or a nonconformity inside the prefix, including a nonzero trailing fill in the bootstrap. | None; the brackets are empty. | For a fill finding the detail names the bootstrap's tape file (`bootstrap at tape_file N has nonzero trailing fill (REM-PARITY 8.1)`). For damage that the walk found, it gives the kind of damage and its position, such as `UnreadableTapeFileHead at LBA 0`. |
| `PROTECTED_CONTENT_FINDING_KIND_NOT_PERFORMED` | The pass over the protected content could not be carried out. | None; the brackets are empty. | The detail gives the reason. The tape is not known to be good, and it is not reported as complete. |

The findings are printed in a fixed order: the pass not performed, then prefix
damage, ParityMap findings, sidecar findings, failed data blocks, and failed
parity shards. With `--json`, the same findings are in a
`protected_content_findings` array with one object per finding, holding the
kind, the address (or null) and the detail, so that a script can tell which
block failed without reading a count out of a `detail` string.

A finding says where the damage is. It does not mean that the tape is lost, and
it repairs nothing. Whether an object that touches a failed block can still be
read depends on the parity: a stripe that has lost no more blocks and shards
than the scheme's `m` (its parity blocks per stripe) can be reconstructed from
the others, and a stripe that has lost more cannot (REM-PARITY Sections 13.4
and 13.5). Remanence has no command yet that reconstructs a block from parity.
Until it does, an object that touches a failed block has to be read from
another copy.

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
the terminal suffix, the prefix and the bootstrap's trailing fill. A no-parity
tape that verifies complete has therefore shown that its structure is sound. It
has not shown that every data block reads, because the checksums that could
show that were never written.

<!-- code-anchor: crates/remanence-parity/src/verify_protected.rs crates/remanence-cli/src/tape_inventory.rs @ 625f7044 -->
## How long a full verification takes

Drive time has two parts: the time to read the bytes, and the time the drive
spends moving to places on the tape.

The bytes set a floor. A full LTO-9 cartridge holds 18 TB natively, which is
18 × 10^12 bytes. A half-height LTO-9 drive reads at 300 MB/s native, so:

```text
18 x 10^12 bytes / (300 x 10^6 bytes/s) = 60,000 s = 16 h 40 min
```

That is the least time a full cartridge written with parity can take. Use the
rating of your own drive if it differs. A parity tape holds `k + m` shards for
every `k` data blocks, so the bytes read exceed the bytes of the objects, and a
tape that is one third full takes about one third as long.

Verification is not one forward pass over the tape, so it does not reach that
floor. In order, it does the following.

1. It reads the bootstrap, then the end of the terminal index, then walks
   every tape file from the beginning of the tape. For each tape file it
   spaces over one filemark and performs at least one LOCATE (the command that
   positions the tape at an address); for an object it performs two.
2. It reads the terminal index replicas and the separation extents between
   them.
3. It preloads the ParityMap's directory of sidecars, then visits objects and
   sidecars in tape-file order. For an eligible epoch it reads the protected
   data forward and keeps the checksums it observes until the sidecar supplies
   the expected ones. It reads the sidecar's metadata and parity at that point.
   An epoch that is not eligible, or only partly buffered, needs fallback data
   reads, which can require a backward seek. The `io` report distinguishes these
   paths.

Contiguous runs are read in fixed-mode batches. An unclean batch is
repositioned and read one record at a time, and metadata probes also use single
reads. Verification does not use the pipelined restore reader, so command counts
alone do not establish its sustained rate. No verification of a full cartridge
has been measured.

The time can be approximated as follows. The formula is useful for comparing
tapes with similar layouts once its two unknowns have been measured on your own
drive:

```text
T = D / R  +  N_files x t_file  +  N_epochs x t_epoch
```

| Symbol | Meaning | Remarks |
| --- | --- | --- |
| `D` | Bytes on the tape, parity included. | A full 18 TB LTO-9 cartridge is about 18 x 10^12 bytes. |
| `R` | Sustained read rate of verification, in bytes per second. | At most the drive's native rate (300 MB/s for a half-height LTO-9 drive); measure it for the drive and layout being checked. |
| `N_files` | Number of tape files. | Objects, plus sidecars, plus the five terminal files, plus the bootstrap and the ParityMap. |
| `t_file` | Time to space over a filemark and position, for one tape file. | Not measured. |
| `N_epochs` | Number of epochs, which equals the number of sidecars. | A full epoch covers 16 GiB of object data at the default settings, but checkpoints can close shorter epochs. For planning, use the actual sidecar count; `rem tape inventory --tape-uuid TAPE_UUID --json` lists the tape's structural files by kind, including parity sidecars. |
| `t_epoch` | Time for one epoch's metadata and parity positioning, including any fallback data seeks. | Not measured. |

`t_file` and `t_epoch` depend on the drive and the cartridge, so measure them
on your own drive. One timed tape cannot determine two unknowns. Verify at least
two tapes whose ratios of tape files to epochs differ, and solve for the two
values from the time that exceeds `D / R`.

Treat the result as an estimate for tapes with comparable layouts. Seek time
grows with the distance travelled, so values measured on a short tape are
uncertain when applied to a full cartridge. The number of tape files matters as
much as the byte count: a tape of many small objects has many tape files, and
the per-file term can then take hours.

The formula leaves out several costs. Loading the cartridge and positioning to
the beginning of the tape add time, and read errors that force retries add more.
The overlaps between objects and epochs, the ParityMap reads and the positioning
for sidecar metadata depend on the layout. Each sidecar's header and index
copies are read twice, once when the copy is checked and once when the index is
acquired. The host and the SCSI adapter must also keep up with the drive. The
phase and fallback counts in the `io` report show how much each of these cost on
a given run.

The [implementation guide](../specs/publication/rem-implementation-guide.md)
says of the walk from the beginning of the tape that it "can take hours". Plan
a full verification of a full LTO-9 cartridge as a task of a working day or
more, with the drive occupied throughout, and run it in a maintenance window.
Verifying a tape just before a restore that will read the same blocks anyway
repeats work.

## The I/O report

The `io` report shows the work the drive did during verification. It helps
explain the elapsed time and makes repeated reads visible. It is diagnostic:
none of its numbers decides the tape's outcome or adds a finding.

The human output has one `io:` summary line and one `io phase` line for each
phase that issued a command, in this form:

```text
io: N ms; LOCATE N SPACE N READ POSITION N; READ single N batch N; records N/N confirmed/requested; N bytes; protected N bytes; backward seeks N; recovered N
io phase PHASE: N.N s, inside commands N.N s, N commands, slowest N.N s
```

With `--json`, `io` is an object in the `rem.tape.verify-index.v1` response for
complete, degraded and recovery-required results. Every count, millisecond
value and address is a decimal string. When the daemon supplies no report, `io`
is `null`; that does not mean no commands were issued. A failed RPC carries no
verification report.

Standalone `rem tape verify --full` includes the same fields in `result.io` and
prints the same human lines. Its `setup` phase starts after the mount, while the
daemon's starts before the readiness probe and the load, so the two `setup`
figures cover different work and are not comparable. An incomplete standalone
verification exits with status 1 and keeps its facts, its I/O report and
`active_fences` in `result`, with `error` set to `full verification incomplete`.

`total` covers all commands dispatched from readiness and setup to the end of
verification. `phases` divides the same work at the existing read boundaries:

| Phase | Work counted |
| --- | --- |
| `setup` | Readiness, drive configuration and the initial bootstrap check. |
| `walk_terminal` | Terminal discovery, the prefix walk, replica checks and separation checks. |
| `parity_map` | Directory preload and explicit ParityMap verification. ParityMap reads made as part of the prefix walk remain in `walk_terminal`. |
| `sidecar` | Metadata acquisition, copy and footer checks, and parity-shard reads. |
| `protected_data` | The forward sweep of protected object blocks. |
| `fallback_data` | Data reads that the sidecar check needs because an epoch was not eligible for buffering or was only partly buffered. |
| `bot_recovery` | The structural recovery walk from the beginning of tape. |

Each phase has a `counts` object with the same fields as `total`:

| Field | Meaning |
| --- | --- |
| `elapsed_ms` | Wall milliseconds while the phase is current, summed across entries and excluding nested phases; `total` covers the whole collection. |
| `command_ms` | Wall milliseconds inside transport dispatches, including unsuccessful attempts. |
| `max_command_ms` | Milliseconds in the slowest command. |
| `command_latency_buckets` | Six command counts for durations <5, [5,20), [20,100), [100,1000), [1000,10000), and >=10000 ms, in that order. |
| `locate`, `space`, `read_position` | Attempted positioning commands. Inline READ POSITION commands issued by LOCATE and SPACE count too. |
| `single_read` | Variable-mode, single-record READ commands. |
| `batch_read` | Fixed-mode READ commands, including a batch that requests one record. |
| `other` | Every other dispatched command, including configuration and readiness commands. |
| `records_requested` | Records requested by READ commands, including retries. |
| `records_confirmed` | Records returned by exact, clean transfers or accepted self-corrected single reads. An unclean batch is not confirmed; its successful single-record retries are. |
| `bytes` | Transferred bytes after sense residue correction when applicable, including configuration responses, failed or unconfirmed transfers, and retries. A transport error without a byte count contributes no known bytes. This is not the tape's stored data size. |
| `hardware_batch_fallbacks` | Unclean hardware batches for which the reader starts a single-record retry. A failed repositioning attempt still counts. |
| `algorithmic_fallback_epochs` | Epochs needing at least one unbuffered data read. These are counted in `fallback_data`, separately from hardware retries. |
| `recovered_reads` | Accepted reads for which the drive reports that it corrected an error. |

In each phase, `command_ms` measures the time inside commands, and
`elapsed_ms - command_ms` the time between commands on the drive actor's
thread. The split tells drive motion and transfer waits apart from host work. It
is a split of wall time, so transport overhead and scheduling waits remain
included. The daemon also logs `remanence_verify_diag` events at INFO level for
each phase that issues a command and for `total`, with the same timings and
command counts.

`io.elapsed_ms` is monotonic elapsed time in the daemon, including readiness and
setup. It excludes time spent waiting for a drive before verification starts,
and client and RPC overhead. It measures the whole verification, not just I/O.

`backward_data_seeks` counts LOCATE commands in the protected sweep whose target
precedes the end position of the previous confirmed protected read. Sidecar and
metadata positioning is outside this count. `protected_bytes` counts confirmed
bytes in that sweep, excluding fallback data. `protected_ranges` lists the
physical intervals read, with an inclusive `start_lba` and an exclusive
`end_lba`. Adjacent intervals may be joined, and overlapping or repeated
intervals remain visible. Zero backward seeks alone proves little, so check the
coverage and fallback counts too. A tape without parity has no protected sweep.

`recovered_lbas` lists the physical addresses of accepted self-corrected reads,
including repeated events at the same address. `recovered_unknown_addresses`
counts such events when the passive position evidence gives no address. Their
sum equals `total.recovered_reads`. No commands are issued just to measure the
report.

A self-corrected read does not itself fail the tape. Verification accepts it
only when exactly one block was transferred, with no incorrect-length, filemark,
end-of-medium or residual-count anomaly. Its bytes go through the same checksum
checks as any other read, and a checksum mismatch remains a finding. The
recovered count records the drive's correction even when those bytes fail a
later check.

## In plain terms

Verifying a tape completely means reading every page of a very long book. The
pages alone take about seventeen hours for a full LTO-9 cartridge, and the drive
also spends time turning back to the index and to the parity pages as it goes. A
tape written without parity has no checksums for its pages, and a page whose
checksum list is missing can be read but not checked. A look at the cover and
the table of contents, which is what `rem tape inventory` does, normally takes
minutes, and it cannot tell you that the pages are readable. "Verified" says
the reading finished. "Complete" says every check that applies to the tape
passed, though pages without a checksum list were not checked. When a page
failed, the tool tells you which one.
