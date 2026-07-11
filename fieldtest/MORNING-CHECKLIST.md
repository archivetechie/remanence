# Field day — morning checklist (read this first, 3 minutes)

## Bring
- [ ] `remanence-fieldtest.tar.gz` (scp it to the HPE server, or USB)
- [ ] Scratch LTO-9 cartridges (they WILL be destroyed): 2 for the core
      append/correctness/benchmark path, 4+ for recovery spares, 6+ for
      destructive lifecycle tests and longer soak
- [ ] The CLN cartridge
- [ ] This checklist; the full story is `RUNBOOK.md`, and the quick same-day
      plan is `TODAY-MSL3040-GUIDE.md`

## First 15 minutes on the server
```bash
tar xzf remanence-fieldtest.tar.gz -C ~ && cd ~/remfield
./scripts/00-preflight.sh
```
Preflight tells you exactly which sudo lines you need (setcap ×4 + sg
access + the tmpfs mount). Run them, re-run preflight until green.

Before dual-drive work, write down the RAM gate: `2 × largest concurrent
object + ring bytes for both drives + OS headroom`, and verify it fits both
tmpfs free space and `MemAvailable`. The daemon must report any removed
`spool-<uuid>.bin` crash orphans before it reports the re-accounted tmpfs
budget. No tmpfs/no RAM evidence means no TIO-5 throughput acceptance.

## The one ordering rule
**Init before daemon; daemon owns the robotics once it's up.**
The scripts enforce it themselves (they refuse when the order is wrong,
and stop/start the daemon around direct-SCSI steps). If a script refuses,
read its message — it says exactly what to run first. Sequence:

```
00-preflight → 01-allowlist → 10-init-pools → 03-bringup → 02-discovery
→ 11-happy-path → 13-append-loop → 12-multiobject → 50-soak start
→ 20/21/22 benchmarks → 30-stewardship (+ --tapealert-probe)
→ 31-cleaning → 32-robotics → 40-faults (each subcommand)
→ 50-soak report → 90-collect-evidence → 91-cleanup
```

## Field wisdom from the dry run (learn from our night)
1. **Used scratch tapes** may refuse init with "FOREIGN remanence
   identity" — that's the overwrite protection working. Use a different
   cartridge; don't fight it.
2. **Never run two daemons / never let anything else touch the library
   mid-day** (backup agent, another admin) — media moved behind the
   daemon's back poisons its inventory. If it happens: `03-bringup.sh
   --stop && 03-bringup.sh` (it re-discovers truth).
3. **If a write hangs >5 min**: `03-bringup.sh --stop` (it now verifies
   the daemon actually died), then bringup again, rerun the step. Capture
   `~/remfield/log/rem-daemon.log` into evidence regardless — a hang IS
   evidence.
4. **A cleaning-cart timeout that fences the drive + raises an alarm is
   a PASS** (the failure protocol working). `31-cleaning.sh --recover`
   guides you out.
5. Everything is rerunnable. When in doubt, rerun the script — inits
   skip already-done tapes, evidence appends.

## If time collapses, run in this order
With 2 scratch data tapes: Phase 0 + 11-happy-path + 13-append-loop +
12-multiobject + 20-bench-write + 21-bench-read + 22-bench-dual carry the
management pitch. Add 30-stewardship + 31-cleaning + 32-robotics before
collection. A 2-tape run has no spare-media margin; if one pool's only tape is
fenced, add media. With 4+ tapes, add 40-faults kill-mid-write + rebuild +
wrong-tape. Keep retire-rebind for the end because it intentionally recycles a
cartridge.

## Before you leave the site
```bash
./scripts/90-collect-evidence.sh   # → evidence-pack-<date>.tar.gz  ← BRING THIS HOME
./scripts/91-cleanup.sh            # prints the sudo-removal lines + tape disposition
```
Also grab `~/remfield/log/` wholesale if anything was weird.

## Numbers to expect (LTO-9, so you can smell trouble live)
- Single-drive sustained write (incompressible, tmpfs source): **~280–310 MB/s**
- Dual-drive aggregate: **~550–620 MB/s**
- TIO-5 gate: write p95 submit gap **≤500 µs**; bench output must name
  `effective_mode=fixed_pipelined`, effective batch, and receive-feed rate
- Run the dual leg as concurrent **restore + append** after both-drive
  readiness pre-warm; compare 1 MiB and 4 MiB `max_sectors_kb` separately
- Read evidence must contain locate/position, transfer, relay, bytes/records,
  batch effectiveness, and populated gap/ioctl cadence fields
- Much lower + preflight showed slow disk → source starvation, not tape
- `rem top` in a spare tmux window is your live truth all day
