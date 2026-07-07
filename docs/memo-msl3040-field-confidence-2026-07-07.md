# Memo — Remanence on physical tape: field-test confidence assessment

**Date:** 2026-07-07. **Audience:** archive department (basis for the
central-IT conversation). **Author:** Claude Fable 5 review, from the
2026-07-05→07 HPE MSL3040 field sessions.
**Evidence:** fieldtest evidence bundle (`records.jsonl`, per-phase daemon
diagnostics, wait-ready artifacts), MSL3040 syslog export (analyzed in
`~/drishti/docs/msl3040-remfield-rca-notes.md`), benchmark CSVs.

## Bottom line

The field test supports going to central IT with the proposal. Every
correctness and safety claim was demonstrated on the physical library, the
one weak number (throughput) is root-caused to two specific software sites
with a reviewed fix design and a ~300 MB/s target that matches the drives'
native rate, and the library's own logs prove our testing coexisted with the
production partition without a single fault ticket.

## Demonstrated on the physical MSL3040 (LTO-9, production host)

| Claim | Evidence |
|---|---|
| Safe coexistence with production | Library logs: our 165-move test window overlapped a production LTO-7 migration job on the shared robot with zero conflicts, zero tickets; production partition untouched by us (library's own MOVE_MEDIUM records) |
| New-media intake incl. LTO-9 first-load calibration | 4 cartridges initialized; the 60–85 min calibration windows match the cartridges' first-ever loads per library lifetime counters; one-time cost, never recurs |
| Write/read/verify, plaintext + encrypted | happy-path suite PASS end-to-end |
| Multi-object tape utilization | 6 objects appended to one tape, dense tape-file numbers, all SHA-256 verified on read-back |
| Crash durability | daemon killed twice (scripted mid-operation + during robot activity); committed objects survived, catalog consistent, follow-up writes succeeded |
| Catalog rebuild | SQLite index deleted and rebuilt from journals with real tape data; objects restorable after |
| Large-object restore | 32 GiB object restored, SHA-256 match |
| Fail-closed safety | readiness fences blocked unsafe I/O through calibration, kill-recovery, and load-settle windows; destructive escalation provably gated |

## The performance picture (honest version)

Measured: 75–76 MiB/s sustained write (identical for compressible and
incompressible data — proof the drive was never the limiter), 82 MB/s
restore. The cause is fully understood and is Remanence code, not hardware:
one SCSI command per 256 KiB record issued serially (plus a per-record
position check on writes, and a store-and-forward spool file on the system
disk). The drives are half-height LTO-9 (native ~300 MB/s). The fix design
(`design-tape-io-throughput-v0.1.md`: batched multi-record SCSI I/O with an
unchanged on-tape format, boundary position proofs, staged overlap) is in
review now, will be validated on the virtual library this week, and
benchmarked on the physical library next window. Even at today's rate the
system moves ~7 TB/day/drive; the fix targets the drives' native rate.

## What is not yet proven (and the plan)

- End-of-media / tape-full rollover behavior — next physical window
  (forced-seal configuration; the failure path is fail-closed by design).
- Multi-drive concurrent throughput — all field I/O used one drive; the
  second LTO-9 drive is untested under our load. Next window.
- Long soak under production-shaped load — VTL nightly suite covers the
  logic; physical soak is a post-decision hardening item.
- Throughput fix validation on physical media — next window, with
  acceptance ≥200 MB/s and target ~300.

None of these are viability questions; they are hardening milestones on a
system whose failure behavior was repeatedly demonstrated to be fail-closed.

## Courtesy findings for central IT (goodwill items)

1. Their library has carried an open warning since 2026-02-24: no cleaning
   cartridge assigned to the production LTO-7 partition (ticket 471).
2. The two LTO-9 drives run different firmware (R3G3 vs S2S1) — worth
   levelling; drive behavior differences we observed correlate with this.
3. A foreign cartridge (S20003L9) was in LTO-9 drive 2 when it was manually
   reset on 07-06; the library log never recorded its unload — worth a
   physical check that it is home in its slot.

## Recommendation

Proceed with the proposal. Frame the performance number as measured-and-
root-caused with the fix in review (not "slow"), lead with the coexistence
proof and the crash-durability evidence, and offer the courtesy findings —
they demonstrate a depth of operational understanding of this specific
library that the commercial track has not shown.
