# Field window 2026-07-07b — MSL3040 + HP server, ~5–6 h, library idle

**Context.** Central IT granted surprise access (their media jobs aren't starting;
nothing is running on the library or the server). Unlike the 07-05..07 window, we
do NOT have to schedule around a live LTO-7 migration — but partition 2 stays
off-limits anyway (the non-interference record is a proposal asset).

**What changed since the last window:** the full TIO arc is on main —
TIO-1 batched I/O core @7c232a5, TIO-2 wired paths + durable fence @2bd6e15,
TIO-3 staged overlap depth-2 @d357cb6, TIO-4 spool placement @d52bc71 — and the
post-TIO clean-slate suite ran 49-green (3 failures RCA'd unrelated,
`~/system/docs/regressions-2026-07-07-suite.md`). The July throughput baseline
(75 MB/s write / 82 read) predates all of this. **This window's job is to measure
what TIO bought, on the same iron.**

**Objectives, strict priority order:**

| P | Objective | Buys |
|---|---|---|
| P0 | TIO throughput ladder on physical LTO-9, same drive + firmware as July baseline | the engineering gate (≥200 MB/s) AND the proposal headline |
| FW | Level drive firmware (coordinated with central IT), one drive at a time, AFTER P0 | removes the R3G3/S2S1 confound; measured fw delta for free |
| P1 | Crash re-proof under the NEW write path (batched sink + fence) | keeps the field-confidence memo honest w.r.t. shipped code |
| P2 | Full-stack end-to-end on physical tape (scenario `rao-live-msl3040`, timeboxed; kit-native fallback) | the proposal's strongest sentence |
| P3 | Dual-drive concurrent load (post-leveling) | migration arithmetic (aggregate MB/s); first physical multi-drive datum |
| P4 | Mount/locate/unload latency distribution (passive, from phase timing) | restore-UX + PFR economics numbers |
| P5 | Soak with whatever remains | drive-hours of confidence at the new throughput |

**Deliberately out of scope:** end-of-media rollover (filling 18 TB ≈ 17+ h —
stays VTL-only; say so in the memo), library-controller firmware (changer reboot
spans both partitions — central IT's hands only), anything touching partition 2,
CLN experiments beyond passive alarm observation.

---

## Prep BEFORE the window opens (from akash, ~45 min, can start now)

- [ ] **Rebuild the kit from current main** (post-TIO): `fieldtest/toolchain/`
      packaging per `fieldtest/RUNBOOK.md` §0 → fresh `remanence-fieldtest.tar.gz`.
      The July tarball is pre-TIO — do not reuse it.
- [ ] Confirm `git -C ~/remanence status` clean and the suite RCA doc's harness
      fixes don't touch remanence (they don't — A/F/Q were harness/sutradhara-side).
- [ ] **Identify the July baseline drive** from last window's evidence bundle
      (which S/N ran the 75/82 numbers — 8031BDC7D1/R3G3 or 8031BDC7DB/S2S1).
      P0 MUST run on that same drive pre-flash.
- [ ] **Firmware**: from central IT contact — target image(s) + confirmation both
      drive part numbers level to ONE version (R3G3 vs S2S1 may be different fw
      branches for different hw revisions; verify on HPE support, don't assume).
      Agree the one-line authorization ("flashing partition-1 LTO-9 drives x2").
- [ ] **P2 plan A feasibility check (10 min, do not sink time):** can akash reach
      the HP server's rem-daemon via SSH LocalForward (pattern proven in
      `~/system/docs/runbook-e2e-off-tailnet.md`; rem certs allow 127.0.0.1 SANs)?
      If cert/config friction appears → commit to plan B now, decide once.
- [ ] Confirm drishti syslog capture target for the library (192.168.90.27) is
      collecting; snapshot a pre-window baseline.
- [ ] Scratch media plan: how many initialized LTO-9 carts remain on the allowlist
      from July? Fresh carts need the ~45-min one-time init — they go FIRST on
      whichever drive P0 isn't using.

## T0 — arrival checklist (~30 min)

1. Verify library + server idle (no other users/jobs) — RUNBOOK.md §0 safety rules.
2. `tar xzf` fresh kit → `00-preflight.sh` → `01-allowlist.sh` (type barcodes —
   everything on the list is destroyable) → `02-discovery.sh`
   (**records pre-flash drive firmware into evidence — required for the fw delta**).
3. tmpfs spool: `sudo mount -t tmpfs -o size=320g tmpfs ~/remfield/spool`
   (TIO-4 config: `daemon.spool_dir` absolute + `spool_tmpfs_ram_budget`).
4. `03-bringup.sh`; `setcap` on the three binaries (rebuild trap!).
5. If fresh carts: kick `09-media-ready.sh` / `10-init-pools.sh` inits on the
   NON-baseline drive now (45 min each, runs unattended).
6. Start/verify syslog capture.

## Timing grid (5.5 h nominal — compress from the bottom if the window shrinks)

| Slot | Drive A (July-baseline drive, old fw) | Drive B (other drive) |
|---|---|---|
| 0:00–0:35 | T0 checklist | T0; media init if needed |
| 0:35–1:45 | **P0: `20-bench-write.sh` + `21-bench-read.sh` ladder** (tmpfs source; capture phase timing; repeat headline point ×3) | (init finishing) → P2 plan-A attempt OR `11-happy-path.sh` warm-up |
| 1:45–2:15 | P0 wrap / analysis | **FW flash drive B** → power-cycle/rediscover → `02-discovery.sh` re-run (fw recorded) |
| 2:15–2:45 | **FW flash drive A** → rediscover → record | **Post-flash re-bench, one ladder point** (fw delta, drive B) |
| 2:45–3:15 | Post-flash re-bench, one point (drive A) | **P2**: scenario `rao-live-msl3040` (plan A, timebox 45 min) or kit-native equivalent: `12-multiobject.sh` + PFR ranged-read pass (plan B) |
| 3:15–4:00 | **P3: `22-bench-dual.sh`** — both drives concurrent (now fw-leveled; aggregate + per-drive + contention) | ← same |
| 4:00–4:45 | **P1: `40-faults.sh` crash subset** — kill daemon mid-BATCHED-write; kill mid-robot-move; rebuild-from-journals; verify catalog + subsequent write | P4 dedicated mount/locate cycles if not already captured |
| 4:45–5:15+ | **P5: `50-soak.sh`** (both drives if possible; runs until hand-back) | ← same |
| last 30 min | **Wrap:** `90-collect-evidence.sh`, syslog snapshot, `91-cleanup.sh`, unload/park all carts, umount tmpfs, leave-as-found walk | ← same |

**Measurement integrity rule:** the P0 numbers that go in the proposal are
same-drive/same-firmware vs the July baseline → attributable to TIO alone.
The post-flash points isolate the firmware contribution. Never mix the two.

## GO/NO-GO and aborts

- Any central-IT activity appears on the library → pause robotics, re-confirm.
- FW flash fails on first drive → STOP the firmware track entirely (one healthy
  drive preserved); continue P0/P1/P5 single-drive; hand findings to central IT.
- P0 lands below the 200 MB/s gate → not an abort: capture full phase timing
  (that's TIO-3/4 tuning data), run the ladder variants, and report honestly.
  The window's job is data, not a pass stamp.
- P2 plan A exceeds its 45-min timebox → drop to plan B without discussion.

## Evidence checklist (what must exist at hand-back)

- [ ] `evidence/` bundle from `90-collect-evidence.sh` (benchmarks with phase
      timing, faults transcripts, discovery pre+post flash, media states)
- [ ] Pre/post firmware versions per drive S/N (02-discovery re-runs)
- [ ] Library syslog capture spanning the window (+ pre-window baseline)
- [ ] P2 record: scenario report (plan A) or kit-native transcript (plan B)
- [ ] Field notes → fold into a `memo-field-window-2026-07-07b.md` +
      `~/proposal` evidence pointers; update `memo-msl3040-field-confidence`
      crash claims to post-TIO code
- [ ] Courtesy findings for central IT (fw leveled ✓/✗, any new alarms, and the
      still-open Feb-2026 cleaning-cartridge warning on their partition)
