# Object sizing for terminal-index tapes

This page explains how the size of the Objects you write affects a tape, and how
the writer decides that an Object still leaves room to close the tape. An
Object is one stored item on tape, written as one tape file followed by a
filemark. The tape's final inventory is written at the end of the tape as three
complete copies, called terminal replicas, so the number of Objects on a tape
has no fixed ceiling. Block size and Object count still determine how much
space the index occupies, and the writer's capacity check counts that space
(see below).

Object size still affects the tape in other ways. It controls tape-file and filemark
overhead, the number of recovery rows in each terminal replica, how often
parity is closed out, catalog scale, and the amount of work needed to retry a
whole Object on another tape.

## What admission reserves

Before an Object moves, the writer proves that its complete commit and the
terminal close fit the conservative tape capacity. The terminal close writes
the three replicas, with a 1 GiB gap between each pair called a
separation extent. For structural-row count
`S`, Object-row count `R`, and fixed block size `B`:

```text
replica_payload_bytes   = 64*S + 256*R
replica_payload_records = ceil(replica_payload_bytes / B)
replica_records         = 1 header + payload records + 1 footer
replica_charge          = replica_records + replica filemark charge

gap_records = ceil(1 GiB / B)  # includes the separation extent header and footer
gap_charge  = gap_records + separation extent filemark charge

terminal_close = parity_closeout
               + 3*replica_charge
               + 2*gap_charge
               + safety allowance
```

For parity-enabled tapes, `parity_closeout` includes any final partial sidecar
and the one external final ParityMap required by a nonempty sidecar directory.
That ParityMap is part of the structural prefix counted by `S`; it is not one of
the five components of the terminal suffix. The no-parity profile has zero
parity-closeout charge but retains the complete terminal suffix.

The supported block sizes are 256 KiB, 512 KiB, and 1 MiB. One default
separation extent is therefore 4096, 2048, or 1024 records respectively, before its separate
filemark charge. The three replicas have the same payload size; their local
headers and footers differ by ordinal and location.

The reserve uses checked `u64` arithmetic. Equality with the available
capacity succeeds; a one-record shortfall refuses the Object before media
motion. The automatic low/high watermarks decide whether a successful Object
leaves the tape open or immediately starts finalization. Manual finalization
below the low watermark uses the same close proof and cannot force an unsafe
fit.

## Why bundle small members

Each stored Object consumes:

- one tape file and trailing filemark;
- one 64-byte structural slot in every terminal replica;
- one 256-byte Object recovery slot in every terminal replica;
- catalog, journal, audit, and verification work; and
- parity closeout overhead when the workload forces a short epoch.

Small files should therefore be bundled into one REM-OBJECT object. Each member
remains individually named and checksummed in
`_remanence/manifest.cbor`, while the tape index spends one Object row on the
bundle rather than one row per member.

Bundling is explicit. Ordinary `rem put A B C` (three input paths) writes three one-member
Objects and therefore three tape filemarks. Build one multi-member object first
and ingest it with `rem put --stored-object` when the files should share one
tape file and one trailing filemark. `--stored-object` goes through the daemon;
standalone `rem put` has no such option, so on a standalone drive every file is
its own Object. Per-member catalog rows, hashes, and
individual restore selection remain available inside that bundle.

Bundling has a floor as well as a ceiling. Every non-empty member starts on a
chunk boundary, so each one advances the stream by at least one tape block
(256 KiB, 512 KiB, or 1 MiB). Members of a few hundred kilobytes or more lose
little to this; a subtree of very small files loses most of its space, and for
that case, and for files the format cannot represent natively, `rem archive
build --rules` can wrap the subtree into one `.remwrap.tar` member with a
`.remwrap.idx` index beside it. REM-OBJECT Core Format Appendix E describes the
convention and how one inner file is recovered from it.

Choose a bundle target from operational needs rather than a format-imposed row
ceiling. Larger bundles reduce filemark, row, and catalog overhead. Smaller
bundles reduce retry cost, staging requirements, and the amount of unrelated
data read for a whole-Object restore. A sensible target should also fit the
available spool and memory budget with room for the configured write pipeline.

## Block-size tradeoffs

A larger tape block reduces the number of records occupied by the fixed one-GiB
separation extents and by a given replica payload, but also enlarges the unit of
read damage and the minimum I/O buffer. Block size is fixed when the tape or
pool is created and cannot change on an already formatted tape. Select it from
drive guidance, throughput, memory, and damage-unit requirements; there is no
fixed Object-count ceiling, but block size still affects index storage and
capacity admission.

## Capacity planning

For planning, include the exact terminal report produced by the writer rather
than a fixed percentage. That report breaks
out parity closeout, one replica, all three replicas, one separation extent,
both separation extents, safety allowance, required tape records, and spool bytes. The shipped
watermarks are the policy defaults; the reserve is the safety proof beneath
them.
