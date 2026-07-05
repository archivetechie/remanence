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

Each write-oriented script needs at least one unused ready tape in its target
pool. `10-init-pools.sh` splits allowlisted data barcodes into `fieldtest-a`
and `fieldtest-b`; with the default split, bring this many scratch LTO-9 tapes:

| Scratch data tapes | Recommended use today |
|---:|---|
| 4 | Smoke/correctness only: Phase 0, `11-happy-path`, stewardship, cleaning, robotics. You can run one extra write-heavy test only if a pool still has an unused tape. |
| 6 | Core management pitch: Phase 0, `11-happy-path`, `20-bench-write`, `22-bench-dual`, then collect evidence. |
| 8 | Core pitch plus exactly one of `12-multiobject`, `21-bench-read`, or one write-heavy fault test. |
| 10+ | Full default Phase 1 and Phase 2 flow with the default pool split. Bring more if you want all fault tests and soak writes. |

If a script says `need ... unused ready tape(s)`, do not override it. Add
allowlisted scratch cartridges and run `10-init-pools.sh` before bringing the
daemon back up, or skip to a lower-priority phase.

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
./scripts/10-init-pools.sh
./scripts/03-bringup.sh
./scripts/02-discovery.sh
```

At this point the daemon owns the robotics. Do not let another backup job,
admin tool, or second Remanence daemon touch the library.

## Priority plan for today

With 4 scratch data tapes:

```bash
./scripts/11-happy-path.sh
./scripts/30-stewardship.sh
./scripts/31-cleaning.sh
./scripts/32-robotics.sh
./scripts/90-collect-evidence.sh
```

With 6 scratch data tapes, run the management-pitch path:

```bash
./scripts/11-happy-path.sh
./scripts/20-bench-write.sh
./scripts/22-bench-dual.sh
./scripts/30-stewardship.sh
./scripts/31-cleaning.sh
./scripts/90-collect-evidence.sh
```

With 10 or more scratch data tapes, run the full correctness and benchmark
path:

```bash
./scripts/11-happy-path.sh
./scripts/12-multiobject.sh
./scripts/20-bench-write.sh
./scripts/21-bench-read.sh
./scripts/22-bench-dual.sh
./scripts/30-stewardship.sh
./scripts/31-cleaning.sh
./scripts/32-robotics.sh
```

Then add recovery tests while unused ready media remains:

```bash
./scripts/40-faults.sh kill-mid-write
./scripts/40-faults.sh rebuild
./scripts/40-faults.sh wrong-tape
./scripts/40-faults.sh retire-rebind
./scripts/40-faults.sh crash-clean
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
- A script refuses because no writable tapes remain: add scratch media or stop
  write-heavy testing. This is a run-plan limit, not a data-path failure.
- The daemon hangs for more than five minutes: run `./scripts/03-bringup.sh
  --stop`, then `./scripts/03-bringup.sh`, and keep the daemon log as evidence.
- Discovery does not show the MSL3040 changer and both drives: stop and fix the
  hardware path before any tape writes.
