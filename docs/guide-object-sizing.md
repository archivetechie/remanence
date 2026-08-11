# Object sizing for terminal-index tapes

REM-PARITY draft.4 no longer stores a cumulative Object directory in a
one-block checkpoint or final bootstrap. The final inventory is streamed into
three complete terminal replicas, so there is no fixed per-tape Object-count
ceiling and no bootstrap row-budget table.

Object size still matters. It controls tape-file and filemark overhead, the
number of recovery rows in each terminal replica, parity closeout frequency,
catalog scale, and the amount of work needed to retry a whole Object on another
tape.

## What admission reserves

Before an Object moves, the writer proves that its complete commit and the
terminal close fit the conservative tape capacity. For structural-row count
`S`, Object-row count `R`, and fixed block size `B`:

```text
replica_payload_bytes   = 64*S + 256*R
replica_payload_records = ceil(replica_payload_bytes / B)
replica_records         = 1 header + payload records + 1 footer
replica_charge          = replica_records + replica filemark charge

gap_records = ceil(1 GiB / B)  # includes the gap header and footer
gap_charge  = gap_records + gap filemark charge

terminal_close = parity_closeout
               + 3*replica_charge
               + 2*gap_charge
               + safety allowance
```

For parity-enabled tapes, `parity_closeout` includes any final partial sidecar
and the one external final ParityMap required by a nonempty sidecar directory.
That ParityMap is part of the structural prefix counted by `S`; it is not one of
the five A/gap/B/gap/C components. The no-parity profile has zero parity-closeout
charge but retains the complete terminal triple.

The supported block sizes are 256 KiB, 512 KiB, and 1 MiB. One default gap is
therefore 4096, 2048, or 1024 records respectively, before its separate
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

Bundling is explicit today. Ordinary `rem put A B C` writes three one-member
Objects and therefore three tape filemarks. Build one multi-member object first
and ingest it with `rem put --stored-object` when the files should share one
tape file and one trailing filemark. Per-member catalog rows, hashes, and
individual restore selection remain available inside that bundle.

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
drive guidance, throughput, memory, and damage-unit requirements; do not select
it to chase the removed bootstrap row budget.

## Capacity planning

For planning, include the exact terminal report produced by the writer rather
than a fixed percentage or an assumed final-bootstrap size. That report breaks
out parity closeout, one replica, all three replicas, one gap, both gaps,
safety allowance, required tape records, and spool bytes. The shipped
watermarks remain the policy defaults; the reserve is the safety proof beneath
them.
