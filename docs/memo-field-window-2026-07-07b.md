# Field memo — MSL3040 window 2026-07-07b (evening/night)

Surprise idle-library window (~6 h, central IT's media jobs stalled; their loading
activity overlapped early evening). First physical validation of the **post-TIO**
stack (main @a53ba22 + fixes below). Operated remotely by Claude via the restricted
`remfield` account (zero sudo, partition-1 device ACLs only, reverse tunnel; all
privileged T0 steps run personally by the owner). Runsheet:
`runbook-field-window-2026-07-07b.md`. Evidence: `~/remfield/evidence/` on the HP
server (collect at teardown).

## Headline results

| Metric | July baseline | Tonight (post-TIO) | Note |
|---|---|---|---|
| Write end-to-end, 32 GiB incompressible, same script | 37.68 MB/s | **60.1 MB/s (+60%)** | matches the frozen design's predicted ~1.6× for this milestone exactly |
| Write end-to-end, 32 GiB compressible | — | 60.7 MB/s | parity (compression off by design) |
| **Pure tape transfer** (daemon diag, 32 GiB) | ~75 MB/s (memo append-path) | **157–164 MiB/s sustained** | >4× the July formal number; batch active (`WRITE6_FIXED_BATCH`), position checks 1/GiB (was per-block) |
| Effective batch | n/a | **4 of 16 requested** | clamped by a 1 MiB sg reserved-buffer grant — kernel/HBA knob, not remanence |
| Read end-to-end (~4 GiB object) | 82 MB/s (different plumbing) | 13.28 MB/s | NOT decomposable: read-side transfer diagnostics missing (gap below); PFR range extracts (early/mid/late) all PASS |
| Crash re-proof (kill mid-batched-append) | passed pre-TIO | **PASS** (see below) | committed data bit-identical; interrupted append never committed; fail-closed throughout |
| Dual-drive aggregate | never attempted | inconclusive tonight | first-load calibration noise + one changer DID_TIME_OUT; overnight soak supplies sustained data instead |

**Path to the ≥200 MB/s gate, fully itemized:** (1) sg reserved-buffer grant 1 MiB →
4+ MiB restores batch 16 (named kernel/HBA knob to chase); (2) close cost ~65 s/object
(synchronous filemark/fence drain; `write_filemarks_immed` currently hardcoded false)
— amortizable and designable; (3) spool staging not yet overlapped end-to-end with
tape write (~370 s of the 572 s wall on 32 GiB). Transfer itself already at 160+.

## Crash re-proof under TIO (P1) — the night's most important result

Sequence: fixture objects written → daemon SIGKILL mid-batched-append → restart.
Outcome: the tape mid-append at the kill was **fenced** (`media_initializing`,
correct fail-closed: the kit script's FAIL verdict was a script vocabulary gap, not
data loss); after operator release with RCA acknowledgement, the committed prefix
object read back **bit-identical** (sha `de82a307…` equal across write-time hash,
catalog locator, read-stream verify, and byte-level compare); the interrupted append
**never appeared in the catalog** (`last_file` unchanged); a fresh write then
committed cleanly. Stronger operator-safety behavior than July's pre-TIO pass.

## Lifecycle firsts on physical media

- **retire → re-init**: July identities on all 4 carts retired with loud copy
  accounting (9 + 2 objects correctly flagged last-copy-lost), then plain-level
  re-init — the tape-rebind lifecycle's first physical exercise.
- **Quarantine fence lifecycle**: raised on interrupted transport (colleague's
  magazine pull mid-move), inspected (`quarantine show`), released with `--ack` +
  `--after-settled-inventory`. Also raised/handled around every first mount
  (LTO-9 load calibration, TUR 02/04/01) — correct, but see UX finding.

## Operational findings (feed designs/backlog)

1. **Magazine interlock spans partitions**: any magazine pulled anywhere pauses the
   single accessor for ALL partitions (sense 3B/12; later a DID_TIME_OUT mid-move).
   Production coexistence with the D2/LTO-7 partition must treat magazine-out as a
   transient, retryable robot state. the owner's ask: surface WHICH magazine (correlate
   drishti library syslog) in `rem top` + console as a `motion-paused` state.
2. **Sticky fences need an auto-resolve path**: 3 of the night's fences required
   manual release after conditions had objectively settled (wait-ready green).
   Fence UX: auto-resolve on settled re-inventory, or a `quarantine resume` verb
   integrated into wait-ready.
3. **Daemon world-model drift**: after the read bench left media mounted and the
   daemon restarted, placement selected a source slot that was physically empty
   (3B/0E). Bringup-time drive-occupancy reconciliation + the queued lazy-dismount
   design should cover this; tonight's traces are the fixture.
4. **Read-side transfer diagnostics missing**: write path logs full phase/batch/
   throughput diag; read path logs only prepare phases. Blocks read-perf RCA.
5. **First-load calibration vs script patience**: every cart's first mount raises a
   readiness fence that outlives the bench scripts' 3 short retries; scripts should
   use the prescribed `wait-ready --resume … --wait`.

## Kit defects found & status

| # | Defect | Status |
|---|---|---|
| 1 | Fresh kit lacks `state/` tree (sqlite cannot create) | fixed procedurally; script fix pending |
| 2 | Dry-run opens catalog read-only ⇒ fresh kit needs `rem-debug catalog reset` bootstrap | script fix pending |
| 3 | Unload guard treats rem's new self-draining dry-run as failure | **fixed in repo** (fieldtest commit) |
| 4 | Init script predates `needs-explicit-rebuild` decision vocabulary | script fix pending |
| 5 | Pool-rule validation counts retired tapes' missing copies (blocked retire under fresh config) | **candidate rem bug** — verify + file |
| 6 | Crash-test verdict conflates fence-refusal with data loss | script fix pending |
| 7 | `remfield-io` ships as source; kit packaging must build it | packaging fix pending |
| 8 | tmpfs spool mount needs `uid=remfield,gid=remfield` (runbook updated) | done |
| 9 | Dual-bench legs race first-load calibrations; needs pre-warm or resume-wait | script fix pending |

## Courtesy findings for central IT

- Firmware leveling deferred (no HPE entitlement tonight): LTO-9 target **S2SD**
  both drives; library 3350→**3370** (their hands). Drive 2 (S2S1) took a visibly
  long first-load calibration — side-by-side argument for leveling.
- The Feb-2026 cleaning-cartridge warning on their partition remains open.
- Their "media won't start" complaint is consistent with what we observed from the
  shared accessor's behavior; happy to share syslog analysis.

## Still open (next window)

Validation of the ≥200 gate after the sg-grant + close work; dual-drive clean
aggregate; end-of-media rollover (physically infeasible in short windows — needs a
dedicated long window or stays VTL); soak report + teardown tomorrow morning
(keys self-expire 2026-07-08 12:00).

## Addendum (same night, REVISED after source verification) — the 1 MiB/command cap is real and hardcoded; the ~160 equilibrium is the *synchronous submission tax* on top of it

*(Earlier framing "platform ceiling, not software — proven three ways" overstated
the evidence; corrected below after reading the driver source.)*

Per-command arithmetic from tonight's diagnostics: tape accepts commands strictly
serially; measured cadence ≈6.2–7.2 ms/command at 1 MiB/command ⇒ ~160 MB/s.
**Source-verified**: the in-kernel smartpqi driver hardcodes
`PQI_MAX_TRANSFER_SIZE = 1 MiB` (`drivers/scsi/smartpqi/smartpqi.h`, v5.14/RHEL9
lineage) — exactly the observed sg reserved-buffer grant. Not firmware, not a
module parameter (the vendor tree's `limit_xfer_size_to_1MB` defaults OFF and
targets logical volumes — unrelated). Immovable without recompiling.

**But 1 MiB/command is only a ceiling for synchronous submitters.** Issued
back-to-back (the st-driver buffered path mainstream backup software uses,
inter-command gap ~0.1 ms), 1 MiB commands stream LTO-9 at native rate through
this same controller — which is why the field isn't full of complaints and HPE
lists the E208e for tape attach in good conscience. Our 6.2 ms cadence = ~1 ms
wire + **~5 ms submission gap, and that gap is rem's** (synchronous SG_IO loop,
next-batch prep in the critical path). Their splitter-cable theory remains
refuted (12 Gb/s per lane).

*(Corrected 07-08: Miria does NOT share rem's mechanism — its MM device config
references `/dev/nst0–3` for data (the st path) + `/dev/sg*` for control. Its
167–223 MB/s loss is application-layer, per its own October tests — digest+dedup
drops ~510→~215 MB/s — and IT's own words: "the software layer is the
bottleneck".)*

**MORNING VERDICT (07-08, dd battery, same drive + card):** kernel st path
sustained **246 MB/s @ bs=1M** and **293 MB/s @ bs=256K** (16 GiB each,
incompressible, tmpfs source, AOX031 sacrificed + retired) — ≈ HH LTO-9 native
rate. **The E208e is fully vindicated; no HBA purchase needed.** The entire rem
gap (160 vs 293) is submission cadence — TIO-5's target, free, on existing
hardware. st-source behavioral review queued (deferred-error machinery for
immediate filemarks, UA auto-retry, EW/EOM semantics, timeout ladders).

**Corrected remedy ranking:**
1. **TIO-5 — pipelined submission (software, free, likely sufficient):** stage
   the next batch while the current command flies; issue at completion with zero
   prep in the critical path. Ordering-safe (still one command on the wire).
   Cadence → ~1.3 ms ⇒ feed ~700 MB/s ⇒ **drive-limited ≈300 on the existing
   HPE card.** Next-arc design item.
2. **Broadcom 9500-16e (~₹40–60k) — optional insurance, no longer required:**
   mpt3sas permits 4–8+ MiB commands (forgiving cadence math) + 4 external ports
   for migration drive scale-out. Diligence unchanged (MSL3040 HBA matrix;
   HPE-branded equivalent for procurement optics).

**Morning discrimination battery (before any purchase):** (a) batch sweep 1/2/4
at fixed bytes — flat ms/command ⇒ fixed submission overhead confirmed; (b) raw
st-driver dd, large bs, allowlisted scratch cart — ~280–300 through the same
E208e fully vindicates the card and localizes the fix in software (needs setfacl
on the /dev/nst node — added to privileged asks); (c) LSI card swap demoted to
optional confirmation. Drive-height note stands (HH LTO-9 native ~300; IT's 400
expectation was FH spec).

**Addendum 2 — Miria AER log finds (same night):** the datamover's Atempo bundle
contains (a) an event log wall of "Default ACL not supported" errors — the defect
behind their silently-misrouted production jobs (Recycler incident), captured in
the vendor's own diagnostics; (b) Media Manager logs with ~8.2k error lines
(10% of the log), dominated by "SCSI command failed, SCSI driver failure (08h)"
polling **drive 8031BDC7D1 — a rem-partition drive**: Miria's config registers all
four drives across both partitions. Caveat: Debug-level poll failures, possibly
reservation/busy. Coexistence ask for central IT: scope Miria's drive registration
to their partition. No throughput data exists in Miria's logs (MM does not log
rates); the email-reported tests remain the throughput record.

## Window close-out (07-08 morning)

**Media ledger at hand-back** (all 5 carts in home slots, drives empty, daemon
stopped): AOX030 = rem 256 KiB, ~5 objects (bench/crash fixtures); AOX032 = rem
256 KiB, 7 objects incl. crash-test evidence + one stray 100 MB morning object
(selector won a fight — see below); AOX034 = **fresh-init at 4 KiB blocks**
(geometry acceptance proven; empty); AOX031 = **retired, contains dd-test
garbage** — awaiting an erase path (finding #11: rem's refuse-clobber ladder is
correct but has no sanctioned erase verb for recycling foreign-data tapes);
CLNU01L9 untouched.

**RAOM (rao-live-msl3040) status:** scenario remains gated — the harness rem
seam drives a local CLI (cannot reach a remote library), so scenario-green needs
harness-on-server (deferred deployment exercise). Acceptance CONTENT: 256 KiB
leg amply proven all window; 4 KiB leg HALF-proven (fresh-init accepted at
block_size=4096; write/restore blocked on pool definition + member re-init —
~10-minute unblock next window, precisely documented). Also learned: daemon pool
writes need pool DEFINITIONS not just placement rules, and tape selection cannot
be steered per-tape (remfield-io lacks --barcode; selector prefers its own
ranking over mounted state — small tooling gap #12).

**Soak post-mortem:** 573 cycles, 0 ok — first-mount fence stuck and the loop
retried blindly all night (kit defect #10: soak needs the wait-ready/resume
wrapper). The fence refused correctly every single time; no data was written
wrongly. Mirror-lesson to the Miria retry-wall finding, with the fail-closed
difference in our favor — but the script-side pattern is the same and gets fixed.

**Evidence:** `~/remfield-evidence/2026-07-07b/` on akash (evidence-pack
20260708 + bench.csv + full logs); library syslog export = the owner, at teardown.
