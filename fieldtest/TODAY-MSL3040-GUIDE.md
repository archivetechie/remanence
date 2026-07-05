# Same-day MSL3040 field-test guide

Use this on July 5, 2026 when you have operator access to the physical HPE
MSL3040. The goal is to get the highest-value Remanence evidence before the
hardware window closes.

## What this suite covers

- Physical discovery: changer, slots, drives, serials, firmware, and SCSI
  visibility.
- Safety: barcode allowlist, non-allowlisted media refusal, and non-clobber
  tape identity checks.
- Data path: archive build, tape write, catalog lookup, tape read, SHA-256
  fidelity, range restore, encrypted object write/read/verify.
- Same-tape append: repeated independent object writes to one pool, dense
  tape-file numbers, read-back SHA-256 verification for every appended object.
- Capacity and speed: incompressible writes, compressible writes, read timing,
  range-read timing, and dual-drive aggregate write.
- Operations: drive catalog, health snapshots, TapeAlert probe, cleaning
  cycle, robotics moves, and IE-port import/export if available.
- Recovery: daemon kill during write, catalog rebuild from journals,
  retire/rebind, wrong-tape overwrite refusal, and crash-mid-clean if a CLN
  cartridge is present.
- Evidence: every step appends `evidence/records.jsonl`; `90-collect-evidence`
  builds a tarball and summary before you leave.

## Media budget

Daemon write scripts now need at least one appendable ready tape in their
target pool. A used ready tape is acceptable and expected because each daemon
write appends a new independent object after the committed prefix. `10-init-pools.sh`
splits allowlisted data barcodes into `fieldtest-a` and `fieldtest-b`; bring
this many scratch LTO-9 tapes:

| Scratch data tapes | Recommended use today |
|---:|---|
| 2 | Core append/correctness/benchmark path: run `10-init-pools.sh --count 2`, all Phase 1 scripts, `20-bench-write`, `21-bench-read`, `22-bench-dual`, stewardship, cleaning, robotics, collect evidence. |
| 4 | The 2-tape path plus spare media for a fenced tape, one fresh-media comparison pass, and `kill-mid-write`, `rebuild`, `wrong-tape`. |
| 6 | The 4-tape path plus retire/rebind at the end, longer soak, and reruns without pausing for media. |
| 10+ | Exhaustive run with destructive tests, repeated benchmark passes, and extra margin for hardware or operator problems. |

If a script says `need ... appendable ready tape(s)`, a used ready tape is
acceptable by design. If a script specifically says `unused ready tape(s)`, do
not override it. Add allowlisted scratch cartridges and run `10-init-pools.sh`
before bringing the daemon back up, or skip to a lower-priority phase. The
2-tape path has no spare-media margin: if either pool's only tape gets fenced,
stop and add media. `10-init-pools.sh --count 1` is not supported for the core
path because `11-happy-path.sh` needs both `fieldtest-a` and `fieldtest-b`.

Dry-run note: the previous `/home/user/remfield-dryrun` run stopped at
`20-bench-write` with "pool fieldtest-a has no writable tapes" because the
pre-append suite required a fresh cartridge. The current suite should append
the benchmark object to a ready used tape instead.

## Fast run order

Run these commands from `~/remfield` on the HPE server.

```bash
tar xzf remanence-fieldtest.tar.gz -C ~
cd ~/remfield
./scripts/00-preflight.sh
```

Apply only the sudo changes that preflight prints. Re-run preflight until it is
green. Then physically load the scratch data tapes plus the CLN cartridge.

```bash
./scripts/01-allowlist.sh
./scripts/10-init-pools.sh        # use --count 2 if you brought two data tapes
./scripts/03-bringup.sh
./scripts/02-discovery.sh
```

For a two-data-tape core run, use `./scripts/10-init-pools.sh --count 2`.

At this point the daemon owns the robotics. Do not let another backup job,
admin tool, or second Remanence daemon touch the library.

## Priority plan for today

With 2 scratch data tapes, run the core path:

```bash
./scripts/11-happy-path.sh
./scripts/13-append-loop.sh
./scripts/12-multiobject.sh
./scripts/20-bench-write.sh
./scripts/21-bench-read.sh
./scripts/22-bench-dual.sh
./scripts/30-stewardship.sh
./scripts/31-cleaning.sh
./scripts/32-robotics.sh
./scripts/90-collect-evidence.sh
```

With 4 or more scratch data tapes, add the highest-value recovery tests before
collection:

```bash
./scripts/11-happy-path.sh
./scripts/13-append-loop.sh
./scripts/12-multiobject.sh
./scripts/20-bench-write.sh
./scripts/21-bench-read.sh
./scripts/22-bench-dual.sh
./scripts/40-faults.sh kill-mid-write
./scripts/40-faults.sh rebuild
./scripts/40-faults.sh wrong-tape
./scripts/30-stewardship.sh
./scripts/31-cleaning.sh
./scripts/90-collect-evidence.sh
```

With 6 or more scratch data tapes, start soak after correctness and run the
destructive media lifecycle test at the end:

```bash
./scripts/11-happy-path.sh
./scripts/13-append-loop.sh
./scripts/12-multiobject.sh
./scripts/50-soak.sh start
./scripts/20-bench-write.sh
./scripts/21-bench-read.sh
./scripts/22-bench-dual.sh
./scripts/30-stewardship.sh
./scripts/31-cleaning.sh
./scripts/32-robotics.sh
./scripts/40-faults.sh kill-mid-write
./scripts/40-faults.sh rebuild
./scripts/40-faults.sh wrong-tape
./scripts/40-faults.sh retire-rebind
./scripts/40-faults.sh crash-clean
./scripts/50-soak.sh report
./scripts/90-collect-evidence.sh
```

`crash-clean` is real-iron only and needs the CLN cartridge path to be usable.
It is fine to skip it if cleaning operations already consumed the practical
window.

## What good output looks like

Live script output should show `[PASS]` lines. The durable record is:

```bash
tail -n 50 ~/remfield/evidence/records.jsonl
cat ~/remfield/evidence/bench.csv
```

Before leaving the site, always collect the pack:

```bash
./scripts/90-collect-evidence.sh
./scripts/91-cleanup.sh
```

Bring back `~/remfield/evidence-pack-YYYYMMDD.tar.gz` and the whole
`~/remfield/log/` directory if anything looked odd.

## Stop conditions

- A script refuses because media is not allowlisted: fix the allowlist or skip.
  Never use a production barcode.
- A script refuses because no appendable ready tapes remain: add scratch media
  or stop write-heavy testing. This is a run-plan limit, not a data-path
  failure. On a 2-tape run, this usually means one pool lost its only tape.
- The daemon hangs for more than five minutes: run `./scripts/03-bringup.sh
  --stop`, then `./scripts/03-bringup.sh`, and keep the daemon log as evidence.
- Discovery does not show the MSL3040 changer and both drives: stop and fix the
  hardware path before any tape writes.
