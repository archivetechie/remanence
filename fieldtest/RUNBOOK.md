# Remanence field test — HPE MSL3040 + 2× LTO-9 — one-day runbook

**Target:** RHEL 9 HPE server, SAS-attached MSL3040, 2× LTO-9 drives,
2 scratch LTO-9 cartridges for the core append/correctness/benchmark path,
4+ if you want spare media for destructive recovery and fresh-media comparison,
plus 1 CLN cleaning cartridge.
**Duration:** 12 hours, phased, with GO/NO-GO gates and skip pointers.
**Prime directive:** every result lands in `evidence/` automatically —
the day produces a management-ready test record, not just a feeling.

---

## 0. Before you start (10 min, read fully)

### Safety rules on the semi-production server

1. Everything lives under **`~/remfield/`** (state, spool, logs,
   evidence). Nothing is installed; no udev, no systemd, no packages.
2. The **complete sudo surface** is:
   ```
   sudo setcap cap_sys_rawio+ep ~/remfield/bin/rem-daemon   # SCSI passthrough
   sudo setcap cap_sys_rawio+ep ~/remfield/bin/rem          # direct-SCSI verbs
   sudo setcap cap_sys_rawio+ep ~/remfield/bin/rem-debug    # break-glass verbs
   printf 'remfield soft memlock unlimited\nremfield hard memlock unlimited\n' | sudo tee /etc/security/limits.d/remfield.conf
   sudo prlimit --pid <tmux server pid> --memlock=unlimited:unlimited
   ```
   plus read access to `/dev/sg*` (usually via `sudo usermod -aG tape $USER`
   + re-login, or `sudo setfacl -m u:$USER:rw /dev/sg*` for the day).
   `00-preflight.sh` tells you exactly which setcap and sg lines you need.
   The two memlock lines raise the locked-memory limit of the field account.
   The `limits.d` file applies only to a new login shell, so log in again
   after writing it. An ssh ControlMaster connection opened before the change
   still carries the old limits into every session it multiplexes; close it
   with `ssh -O exit <host>` before you log in again. `prlimit` raises a tmux
   server that is already running, whose pid
   `tmux display-message -p '#{pid}'` prints when run as the field account.
   `03-bringup.sh` refuses to start the daemon under a limit below 1 GiB.
3. **The barcode allowlist is the interlock.** `01-allowlist.sh` asks
   you to type the scratch barcodes (and the CLN barcode). Every script
   refuses to initialize, write, move, or erase any cartridge not on
   that list. Production cartridges in the library are therefore
   untouchable by the kit. Double-check the list — everything ON it
   will be DESTROYED.
4. If anything looks wrong on the box (other users, jobs on the
   library, drives in use), STOP and resolve before continuing.

### Lessons from 2026-10-26

The October 2026 window on the MSL3040 produced these operational rules.

1. The daemon inherits the locked-memory limit of the tmux server it is
   started in. Only the read reservoir is locked memory: under the field
   account's default of 8 MiB a restore's reservoir cannot grow past 8 MiB,
   so its drive reader parks more often. Run the two memlock lines of the
   sudo surface above before bring-up; `03-bringup.sh` refuses to start the
   daemon under a limit below 1 GiB and prints both commands.
2. The lock limit does not set the write batch. That night's writes ran with
   an effective batch of 4 blocks against a configured 16, which is what a
   1 MiB sg reserved buffer gives with 256 KiB blocks, so check
   `effective_batch_blocks` in the bench output against the
   [host I/O setup](#host-io-setup-for-batched-tape-commands) before
   accepting throughput data.
3. Insert new cartridges into empty slots that are not any loaded tape's
   home slot, or drain the drives first. A cartridge placed in a loaded
   tape's home slot strands that tape in its drive when the drain tries to
   return it, and the fence left behind makes the next `10-init-pools.sh`
   fail with an unexpected refusal.
4. A daemon killed mid-unload leaves the cartridge unthreaded and the drive
   reporting not ready until it receives a LOAD; `rem tape quarantine list`
   shows the resulting fence as `media_initializing`, and waiting does not
   clear it. The drain now sends one LOAD with `sg_start --load` (sg3_utils
   must be installed) before it skips the bay, and `03-bringup.sh --stop`
   waits a minute for a graceful stop before the hard kill.
5. Never rerun `10-init-pools.sh` over initialized tapes without the rerun
   guard, which skips every barcode that has a PASS init record from this
   kit in `evidence/records.jsonl`. `FIELD_INIT_ALLOW_REINIT=1` is the
   override; it sends those tapes through the clobber route again and
   destroys what they hold.
6. After any hard kill of the daemon, check `rem top --once` for a bay shown
   busy with no I/O. That is the stall in item 4, and a LOAD to the drive
   clears it (see
   [the troubleshooting guide](../docs/guide-troubleshooting.md#a-drive-stays-not-ready-after-a-daemon-crash)).

### Unpack + binary check (5 min)

```
tar xzf remanence-fieldtest.tar.gz -C ~ && cd ~/remfield
./scripts/00-preflight.sh          # probes everything, writes evidence/00-preflight.json
```

Preflight verifies: binaries execute on this glibc (plan A = RHEL9-built,
verified in a RHEL 9 container before packaging; plan B = build from
`toolchain/` — offline-capable, ~20 min, `toolchain/README.md`), `/dev/sg*`
visibility + permissions, changer + 2 drives present, and storage layout
per the tier plan below.

### Storage tiers (decided 2026-07-04 with operator)

| What | Where | Why |
|---|---|---|
| **Daemon spool + benchmark payloads** | **tmpfs** at `~/remfield/spool` (`sudo mount -t tmpfs -o size=320g tmpfs ~/remfield/spool`) — the server's 512 GB RAM is idle | source must outrun LTO-9 (~300 MB/s ×2); tmpfs (~10 GB/s) guarantees the TAPE is what benchmarks measure. This is also the wear-safe path: feeding below the LTO-9 speed-matching floor makes the drive back-hitch. The daemon config generated by `03-bringup.sh` sets `daemon.spool_dir` to the absolute `~/remfield/spool` path and `daemon.spool_tmpfs_ram_budget = "64GiB"`; overflow-to-disk is intentionally not implemented. |
| **Catalog / journals / audit / evidence / logs** | root disk (`~/remfield/state`, `~/remfield/evidence`) | must honestly survive the daemon-kill and rebuild-from-journals tests; tiny (MBs) |
| **Real video files (PFR set)** | copy a subset (≤150 GB) into tmpfs for the clean-source realism pass; ALSO archive once directly from the ZFS/QSAN mount | the tmpfs pass = tape capability on real broadcast formats + ranged reads (the partial-file-restore story); the QSAN pass is recorded as **"current production storage path (known-degraded QSAN config)"** — an INFO benchmark, not a remanence number. The gap between the two lines is the QSAN-fix argument |

The slow ZFS-over-QSAN mount is deliberately NOT used as a benchmark
source anywhere else. If sudo policy prefers a separate mountpoint, mount
tmpfs at `~/remfield/ram` and symlink `~/remfield/spool` to it before
`03-bringup.sh`; dangling spool symlinks are startup errors by design.

**DL385 dual-drive RAM gate (mandatory):** before starting the daemon, record
`MemAvailable`, the tmpfs size/free bytes, the largest object used by either
concurrent leg, configured ring depth, effective batch bytes, and drive count.
The admitted budget must cover `2 × largest_object + ring_depth ×
effective_batch_bytes × 2 drives + OS headroom`. The daemon's acknowledged
`spool_tmpfs_ram_budget` must fit inside that amount and tmpfs free capacity.
If it does not, reduce object size/concurrency or enlarge the mount; do not let
swap or the root disk turn this into a tape benchmark. Startup removes and
evidences owned `spool-<uuid>.bin` crash orphans before re-accounting this
budget; any reconciliation failure prevents writes from being accepted.

### Host I/O setup for batched tape commands

Before Phase 2, raise and verify the SCSI generic single-I/O ceiling. The
batched WRITE/READ path is capped by the host `max_sectors_kb`/sg reserved
buffer, not by tape block limits.

```bash
cat /sys/module/sg/parameters/max_sectors_kb
echo 8192 | sudo tee /sys/module/sg/parameters/max_sectors_kb
cat /sys/module/sg/parameters/max_sectors_kb
```

Use `/sys/module/sg/parameters/max_sectors` if that is the name exposed by the
field host's sg driver. The verification line must show the raised value before
benchmarking. The daemon logs the effective batch size at drive open; if it is
silently clamped below 16, stop and fix this before accepting throughput data.

### Scratch media budget

Daemon write scripts need at least one appendable ready cartridge in their
target pool. A cartridge that already holds committed Remanence objects is
valid and expected; the suite now appends independent objects instead of
burning a fresh cartridge per object. `10-init-pools.sh` splits allowlisted
data barcodes between `fieldtest-a` and `fieldtest-b`; scripts fail early with
a `media-budget` record if their target pool cannot satisfy the relevant media
condition. Unused ready media is still needed for explicit fresh-media
experiments and for destructive flows such as retire/rebind.

| Scratch data tapes | Practical plan |
|---:|---|
| 2 | Phase 0 with `10-init-pools.sh --count 2`, all Phase 1 correctness scripts, `20-bench-write`, `21-bench-read`, `22-bench-dual`, stewardship, cleaning, robotics, then collect evidence. |
| 4 | The 2-tape path plus spare media if a cartridge gets fenced, one fresh-media comparison pass, and the `kill-mid-write`, `rebuild`, and `wrong-tape` recovery tests. |
| 6 | The 4-tape path plus retire/rebind at the end of the day, longer soak, and reruns without stopping for media. |
| 10+ | Exhaustive run with destructive tests, repeated benchmark passes, and extra margin for operator mistakes or hardware faults. |

If appendable media runs out, add allowlisted scratch tapes and rerun
`10-init-pools.sh` before restarting the daemon, or skip lower-priority
write-heavy phases. The 2-tape path has no spare-media margin: if either pool's
only tape gets fenced, stop and add media. `10-init-pools.sh --count 1` is not
supported for the core path because `11-happy-path.sh` needs both
`fieldtest-a` and `fieldtest-b`. Do not try to fill an 18 TB cartridge today
by brute force; the default payload sizes prove append, positioning, catalog,
readback, and throughput without spending the whole window writing capacity.

---

## Phase 0 — bring-up + discovery (H0–H1)

| Step | Command | Manual? |
|---|---|---|
| 0.1 | `./scripts/00-preflight.sh` | fix whatever it flags |
| 0.2 | 🖐 load scratch tapes + CLN cart into magazine slots; note barcodes | physical |
| 0.3 | `./scripts/01-allowlist.sh` | type the barcodes |
| 0.3a | `./scripts/09-media-ready.sh --count 4 --no-wait` — no-move readiness sweep over allowlisted media in the selected library. Loaded drives are polled; slot-only tapes are recorded as `SKIP/not_loaded`. Use `--condition-all` to sweep every visible allowlisted data barcode. | |
| 0.4 | `./scripts/10-init-pools.sh` — readiness-aware drive drains, initializes scratch into 2 pools. **Runs BEFORE the daemon** (direct-SCSI; the script refuses if a daemon is up — one owner of the robotics at a time) | |
| 0.4a | `./scripts/09-media-ready.sh --resume <operation_id>` — only if init exits 10 / reports media not ready, or the library UI shows Calib/initializing for an already-loaded tape. Leave the cartridge in the drive; do not move/unload/retry until this returns ready. | |
| 0.5 | `./scripts/03-bringup.sh` — config, daemon start (tmux window `rem`), verifies initialized tapes visible in the daemon catalog | |
| 0.6 | `./scripts/02-discovery.sh` — libraries, slots, drive identity via the daemon | |

**Rule the kit enforces throughout:** direct-SCSI verbs (`tape init`,
`rem-debug` moves, retire) never run while the daemon is up — scripts
stop/start it around such steps themselves. Media moved behind a live
daemon's back poisons its cached inventory.

**GO gate:** discovery shows the MSL3040 changer, BOTH drives with real
serials (`identity_source` should be a DVCID variant, not `Derived`),
and every allowlisted barcode visible in a slot. Evidence:
`evidence/02-discovery.json` (this alone is a milestone — our own SCSI
stack, no mtx, against real iron).

**If blocked:** drives visible but changer missing → check SAS zoning /
`lsscsi -g`; permissions errors → sudo surface above; nothing visible →
hardware path problem, involve whoever owns the server. Do not proceed
past a red gate.

---

## Phase 1 — correctness core (H1–H3)

| Step | Script | What it proves |
|---|---|---|
| 1.1 | `11-happy-path.sh` | archive build → write → catalog → read → **SHA-256 fidelity** → restore, plaintext AND encrypted object |
| 1.2 | `12-multiobject.sh` | many-object archive, manifests, ranged reads (partial restore without reading the whole object) |
| 1.3 | `13-append-loop.sh --mode cycle` and `13-append-loop.sh --mode session` | repeated independent object writes to one pool; same tape UUID, dense tape-file numbers, read-back SHA-256 for every object |

Run the append loop twice and compare the two summary records: `session`
measures append-format behavior plus amortized throughput, while `cycle`
measures full mount-cycle latency plus robotics stress.

**GO gate:** happy-path reports `fidelity: PASS` for both objects.
The Phase 1 scripts are LTO-9-readiness-aware for ordinary daemon I/O:
if a write/read open returns a `media-readiness fence operation=...`, the
script records `*-readiness-blocked-*`, waits on the operation, and retries.
Single readiness waits longer than `FIELD_READY_WARN_SECS` are marked in
evidence and printed as warnings; waits longer than `FIELD_READY_FAIL_SECS`
are hard stops. Terminal, timeout, or transport-unknown readiness results
remain hard stops.
**If blocked:** a single drive failing → continue on the other, note in
evidence; catalog errors → capture `evidence/` + daemon log, then
`91-cleanup.sh --state-only` and retry once from 1.1.

---

## Phase 2 — benchmarks (H3–H5)

| Step | Script | Measures |
|---|---|---|
| 2.1 | `20-bench-write.sh` | acceptance leg 1: single-drive sustained write and p95 submit-gap gate; incompressible AND compressible payloads. The bench prints and records effective mode, effective batch, and gRPC receive→spool feed rate. |
| 2.2 | `21-bench-read.sh` | acceptance leg 4: sustained read plus complete `restore_total` decomposition (locate/position, transfer, relay, cadence/ioctl histograms, batch effectiveness, bytes/records); ranged-read time-to-first-byte follows. |
| 2.3 | `22-bench-dual.sh` | acceptance leg 2: BOTH drives concurrently, one restore + one append through the E208e/smartpqi ring. The script readiness-conditions both drives first (kit defect #9 pre-warm/resume-wait), then records per-leg and aggregate rates. This is the HBA purchase-decision leg. |
| 2.3a | repeat `20-bench-write.sh` once with 1 MiB and once with 4 MiB `max_sectors_kb`/sg grant | acceptance leg 3: determines whether the larger grant is useful margin. Record sysfs before/after and the effective batch printed by the bench. |
| 2.3b | `20-bench-write.sh` output/evidence | acceptance leg 5: capture the `receive_feed_rate_mib_s` gRPC-receive→spool counter in the same window for A7 sizing. |
| 2.5 | `20-bench-write.sh --source <zfs-mount-path>` | the "current production storage path" INFO line: archive real video files directly from the ZFS/QSAN mount (see storage tiers) — labeled degraded-source, NOT a remanence number |
| 2.6 | `11-happy-path.sh --source <tmpfs-video-subset>` | realism pass: real broadcast video files through archive→restore→SHA-256 + ranged reads (the partial-file-restore story) |
| 2.4 | (automatic) | `rem top --once --json` sampled every 5 s during every benchmark → live MB/s evidence + a demo screenshot moment: run `rem top` in a spare tmux window and watch |

TIO-5 acceptance requires tmpfs; root-disk numbers are informational only.
Leg 1 passes at p95 submit gap ≤500 µs and sustained pure transfer ≥280 MB/s
with the 1 MiB grant. Leg 2 should approach twice the measured single-drive
rate, with per-drive cadence retained. For leg 3, run and evidence:

```bash
echo 1024 | sudo tee /sys/module/sg/parameters/max_sectors_kb
./scripts/20-bench-write.sh
echo 4096 | sudo tee /sys/module/sg/parameters/max_sectors_kb
./scripts/20-bench-write.sh
```

Use the equivalent sg parameter name exposed by the host. Each bench must
print `effective_mode=fixed_pipelined`, `effective_batch_blocks=...`, and (for
writes) `receive_feed_rate_mib_s=...`; absence is a failed leg, not a skipped
counter. Leg 4 accepts decomposition, not a promised read-rate improvement:
`phase_sum_ms` must approximately equal `wall_ms`, and cadence/ioctl samples
must be populated for a multi-command restore.

**If short on time and you have 2+ scratch tapes:** run 2.1 and 2.3 only — one-drive write + dual
aggregate are the two numbers management will remember.

---

## Phase 3 — drive stewardship on real iron (H5–H6.5)

This phase closes the three questions the drive-stewardship design left
open for real hardware (O1/O2/O3) — capture is automatic.

| Step | Script | What it answers |
|---|---|---|
| 3.1 | `30-stewardship.sh` | drive catalog rows with REAL serials/firmware; health snapshots — dumps every LOG SENSE page the drives serve (O2) into evidence; correlation rollups populated by the day's sessions |
| 3.2 | `30-stewardship.sh --tapealert-probe` | reads TapeAlert twice back-to-back and records whether flags clear on read (O3) — safe: rem is the only reader today |
| 3.3 | 🖐 + `31-cleaning.sh` | **real cleaning cycle**: registers the CLN cart, runs `rem drive clean` on drive 1, follows the `clean_runs` phase machine live, verifies completion + use-count credit (O1). Repeat on drive 2 if time allows |
| 3.4 | `32-robotics.sh` | move-medium slot↔slot, IE-port export/import of one scratch tape, inventory refresh consistency |

**Note:** if the drive doesn't auto-eject the CLN cart within the
timeout, the script parks the run `needs-operator`, fences the drive,
and raises the alarm — **that is a PASS for the failure protocol**, not
a failure; the script says so. 🖐 remove the cart via the library panel
and run `31-cleaning.sh --recover`.

---

## Phase 4 — faults + recovery (H6.5–H8.5)

The management questions: what happens when it breaks?

| Step | Script | Scenario |
|---|---|---|
| 4.1 | `40-faults.sh kill-mid-write` | starts a large write, `kill -9`s the daemon mid-transfer, restarts, verifies: session marked lost, tape recovered, next write succeeds, catalog consistent |
| 4.2 | `40-faults.sh rebuild` | **catalog rebuild from journals with real tape data** — deletes the SQLite index, rebuilds, verifies every object still restorable + drive history survived (authoritative tables) |
| 4.3 | `40-faults.sh retire-rebind` | retires a written tape, verifies copies flagged missing, re-inits the same barcode as a fresh tape |
| 4.4 | `40-faults.sh wrong-tape` | 🖐 swap two allowlisted tapes between slots (or via `rem-debug move`), then attempt a write — must refuse with an identity mismatch, never overwrite |
| 4.5 | `40-faults.sh crash-clean` | if the CLN cart is still available: kill the daemon mid-clean (once the changer shows the CLN cart in the drive), restart, watch startup reconciliation resolve the run (the DS-M2 crash-resume path on real robotics) |
| 4.6 | `34-verify-throughput.sh` | full verification timing, phase split, command latency and same-tape deltas |

### 4.6 Full verification throughput

Run this leg before and after a binary change on the same tape. The default is
the first `sealed` tape from `rem catalog tapes --json` present in the selected
library's live slots or drives. `sealed` is the catalog's terminal-finalization
state; the catalog has no separate terminal-index flag. An explicit UUID or
barcode must also select a sealed tape in that library.

```bash
./scripts/34-verify-throughput.sh --tape-uuid <uuid>
./scripts/34-verify-throughput.sh --barcode <barcode>
FIELD_CONFIRM_FINALIZE=1 ./scripts/34-verify-throughput.sh --finalize-bench
```

`--finalize-bench` selects the first appendable `fieldtest-a` tape and permanently
removes its appendability. It requires `FIELD_CONFIRM_FINALIZE=1`, records the
exact UUID and idempotency key, and uses `rem tape finalize --wait`. The resolved
binary must offer that command. `FIELD_VERIFY_TIMEOUT_SECONDS` defaults to
14400 and bounds each verification or finalization command. The leg waits for
idle bays using `FIELD_BUSY_WAIT_S` (600) and `FIELD_BUSY_RETRY_INTERVAL_S` (5).
It uses `--ready-wait 15m` when available; otherwise it reads at most 1 MiB from
an object on that tape through the readiness-aware `remfield-io` wrapper.

The 27 Oct baseline on a 36 GB parity tape gives these figures to beat:

| Phase | Elapsed seconds | Inside commands | Between commands |
|---|---:|---:|---:|
| walk_terminal | 398 | 394 | 4 |
| parity_map | 27.5 | — | — |
| sidecar | 184 | 175 | 9 |
| protected_data | 266 | 135 | 131 |
| total | 875 | — | — |

Evidence lives under `evidence/34-verify-throughput/`: `verify-<stamp>.json`,
`verify-split-<stamp>.tsv`, `verify-delta-<stamp>.txt`, and
`verify-diag-<stamp>.jsonl`. Each TSV row includes the tape UUID, decimal MB/s,
six command-latency buckets in milliseconds (`<5`, `5–20`, `20–100`, `100–1000`,
`1000–10000`, `>=10000`), and command counts. The total uses the daemon's total
counters, including setup, rather than adding the rounded baseline phases.
Deltas use the most recent split for that UUID by file modification time and
report elapsed, command and between-command seconds and percentages; a zero
reference has no percentage. PASS requires `verification_state=verified_complete`
and a successful command. Missing timing or daemon diagnostics fails the leg.
The diagnostic file contains only events appended during the command window;
plain tmux lines are preserved in JSON `line` fields. Keep other verification
clients idle during this leg because daemon events do not include a tape UUID.

**Every red result here is still evidence** — capture it, note it, move
on. A refused unsafe operation is a PASS.

---

## Phase 5 — soak + re-verify (H8.5–H10)

- `50-soak.sh start` actually belongs at the END of Phase 1 — it runs a
  mixed write/read/verify workload in the background whenever drives
  are idle between phases, all day. At H8.5, `50-soak.sh report` prints
  each run's cycle, ok and failed counts from `log/soak.log`, then the
  total over every run in it with the number of runs summed
  (`runs=… cycles=… ok=… failed=…`).
- Re-run `11-happy-path.sh` — after every fault above, the plain happy
  path must still pass. This is the stability headline.
- **5.V — full-verification timing** (after the last write of the day, with the soak stopped, before
  evidence collection; it occupies one drive for the whole run). Use a tape initialized with
  `--parity default` (the normal pool tapes from `10-init-pools.sh` are parity-off, and a parity-off tape
  verifies only its structure). Prefer the fullest such tape; finalize it, then run

  ```text
  rem tape verify-index --tape-uuid TAPE_UUID --json > evidence/verify-index-TAPE_UUID.json
  ```

  `TAPE_UUID` is the tape's UUID as printed by `rem catalog tapes`. Then restore every Object on that
  tape (`rem get OBJECT_UUID --tape TAPE_UUID --dest SCRATCH_DIR --json`, one at a time, deleting each
  restored file after its digest check) and time the whole restore as the streaming baseline. Record:
  tape UUID; stored and protected bytes; Object and epoch counts; drive model and firmware; block size and
  read batch setting; the verification's own elapsed time (in the `io` report) and the restore's elapsed
  time; and the complete `io` report. A tape that is not full gives a measured rate, not a full-cartridge
  duration; label any extrapolation to 18 TB as such.

---

## Wrap-up (H10–H12)

1. `./scripts/90-collect-evidence.sh` — collates everything into
   `evidence-pack-<date>.tar.gz` + generates `SUMMARY.md`: test matrix
   (ID / description / PASS-FAIL-SKIP / evidence file) and the
   benchmark tables. Bring this home.
2. `./scripts/91-cleanup.sh` — stops the daemon, removes setcap/ACLs
   (prints the sudo lines), leaves `~/remfield` in place (or
   `--purge`). Tapes: the script prints the disposition list — which
   barcodes now hold test data (relabel or keep as proof), CLN cart use
   count.
3. 🖐 Return the library to how you found it (magazine layout).

## Priorities if the day compresses

**Must (the pitch survives with 2 scratch tapes):** Phase 0, 1.1, 1.3,
1.2, 2.1+2.2+2.3, 3.1, 3.3, 3.4, then collect.
**Should:** 4.1, 4.2 (the two recovery stories), 3.3 (real cleaning).
**Nice:** everything else. The soak runs itself regardless.

## Quick reference

- Daemon window: `tmux attach -t remfield` (window `rem` = daemon log,
  window `top` = live console, window `work` = you).
- Every script: `--help`, and `EVIDENCE_DIR`/`REMFIELD_HOME` env
  overrides. All idempotent — rerunning is always safe.
- Something inexplicable: `evidence/` + `~/remfield/log/` capture
  everything; grab them and continue with the next phase.

### Environment knobs

| Variable | Default | Meaning |
|---|---:|---|
| `FIELD_IO_READY_RETRIES` | `3` | daemon I/O fence retry count |
| `FIELD_IO_READY_TIMEOUT` | `2.5h` | `wait-ready --timeout` used by daemon I/O fence retries |
| `FIELD_IO_READY_POLL` | `30s` | steady-state `wait-ready --poll` used by daemon I/O fence retries |
| `FIELD_READY_WARN_SECS` | `90` | mark a single fence wait with `readiness_warning=true` and print a warning above this duration |
| `FIELD_READY_FAIL_SECS` | `900` | log `FAIL` and abort the daemon I/O retry loop above this duration |
| `FIELD_CLEAN_DRIVE_INDEX` | `0` | position, in the daemon's drive list (`rem drive list --foreign --retired --json`), of the drive that `31-cleaning.sh` and `40-faults.sh crash-clean` clean; set it to leave alone a drive cleaned earlier in the day, which the cleaning cadence guard would refuse |
| `FIELD_CLEAN_TIMEOUT_SECONDS` | `1800` | how long `31-cleaning.sh` follows a cleaning run, and how long `40-faults.sh crash-clean` waits for the CLN cartridge to reach the drive before it records FAIL without killing the daemon |
| `FIELD_INIT_ALLOW_REINIT` | `0` | `1` lets `10-init-pools.sh` re-initialize a barcode that already has a PASS init record from this kit; otherwise the script records SKIP for that barcode |
| `FIELD_BUSY_WAIT_S` | `600` | how long `remfield-io` keeps re-trying a session open that a busy drive bay refused, and how long `22-bench-dual.sh` waits for idle bays before its fixture write |
| `FIELD_BUSY_RETRY_INTERVAL_S` | `5` | seconds between those attempts |
| `FIELD_ALLOW_LOW_MEMLOCK` | `0` | `1` lets `03-bringup.sh` start the daemon although the locked-memory limit it would inherit is below 1 GiB and not unlimited; for a deliberate low-memory test only |
