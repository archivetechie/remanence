# Object sizing and the bootstrap row budget

Every REM-PARITY tape carries a bootstrap block: a single tape block,
rewritten at each checkpoint, that lets a bare tape be read back without
any external database. Among other things the bootstrap holds one row
per stored object — the object's tape-file number, size, and the anchors
a recovery reader needs to find the object's own manifest. The row set
is cumulative: the newest bootstrap describes every object written so
far, so the last readable bootstrap alone rebuilds the tape's object
catalog.

The bootstrap is deliberately limited to one block. A single block
either reads whole or not at all, which keeps recovery free of torn,
partially-readable catalogs; the trade is that the block's size caps the
number of object rows a tape can ever hold. The writer enforces this cap
at admission — before an object is accepted, it proves the enlarged row
set still fits — so a tape can never overflow its bootstrap. What
happens instead is that the tape becomes **object-full**: the writer
refuses further objects and the tape seals, even if physical capacity
remains. Nothing is corrupted, but the remaining metres are wasted.
Sizing objects correctly is therefore an efficiency concern, and this
guide gives the numbers.

## The measured row budget

Measured against the writer's own admission validator (worst-case row
widths, default RS(128,4) scheme, worst-case header fields — the
guaranteed floor the writer enforces; measured at `cac1f240`,
2026-08-05):

| Tape block size | Plaintext objects | Encrypted objects | Remarks |
| --- | --- | --- | --- |
| 256 KiB (default) | 1,636 | 1,077 | Encrypted rows are larger: they carry recipient-epoch identifiers instead of manifest anchors. |
| 512 KiB | 3,275 | 2,156 | Block size is fixed at pool creation and cannot change on a formatted tape. |
| 1 MiB | 6,551 | 4,313 | Larger blocks also enlarge the unit of read damage; the parity scheme adapts to keep the same contiguous-loss tolerance. |

A mixed tape is bounded by the mix; for planning, use the encrypted
column whenever a pool stores any encrypted representation.

## Minimum average object size per generation

Dividing native cartridge capacity by the row budget gives the minimum
*average* stored object size at which a tape fills its media before it
exhausts its rows. At the default 256 KiB block size:

| Generation | Native capacity | Encrypted pools | Plaintext pools |
| --- | --- | --- | --- |
| LTO-7 | 6 TB | 5.6 GB | 3.7 GB |
| LTO-8 | 12 TB | 11.1 GB | 7.3 GB |
| LTO-9 | 18 TB | 16.7 GB | 11.0 GB |
| LTO-10 (shipping media) | 30 TB | 27.9 GB | 18.3 GB |
| LTO-10 (full Gen 10 spec) | 40 TB | 37.1 GB | 24.4 GB |

Large objects are never the problem: a tape of 100 GB video objects
uses a few hundred rows at most. The budget binds when many small
files are stored, and the answer there is bundling — packing small
files into one object, each member individually named and checksummed
in the object's internal manifest (`_remanence/manifest.cbor`), so
per-file identity survives on tape while the bootstrap spends one row
per bundle.

## How many members per bundle

For a bundle-heavy pool, the bundle flush size must reach the minimum
average above. In member counts, on LTO-9 with encryption:

| Typical member | Member size | Members per bundle (minimum) |
| --- | --- | --- |
| Photograph | 5 MB | ~3,300 |
| RAW image / audio file | 25 MB | ~670 |
| Short video clip | 100 MB | ~170 |

The general rule: **members per bundle ≥ (native capacity ÷ row
budget) ÷ average member size.** We recommend targeting about 1.5×
the minimum bundle size rather than the exact floor. Three things eat
the margin: the writer reserves a full batch of worst-case rows ahead
of each checkpoint, so the practical ceiling sits slightly below the
measured budget; the final bundle of an ingest is usually partial; and
mixed pools dilute the average unpredictably.

## If the budget is ever too tight

Two relief paths exist, in order of preference. First, a larger tape
block size at pool creation — the budget scales linearly with block
size, and 512 KiB or 1 MiB are supported and probed by the recovery
reader today. Block size should be chosen on its own merits (drive
throughput, vendor guidance, damage-unit size), with the row budget as
one input; it cannot be changed on an already-formatted tape. Second,
the REM-PARITY specification reserves an external overflow carrier for
object rows under a future `schema_minor`; no revision defines one
yet, and a writer must never improvise around the one-block rule.

In plain terms: each tape keeps its own one-page table of contents,
and a page holds a fixed number of lines. Films use few lines, so they
never worry; photographs must be boxed up so that each box — not each
photograph — takes a line, and every box carries its own packing slip
inside. This guide says how many lines a page has, and therefore how
big the boxes need to be for each size of shelf.
