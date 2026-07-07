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

## Addendum (same night) — the throughput ceiling is the platform's, proven three ways

Per-command arithmetic from tonight's diagnostics: tape accepts commands strictly
serially; measured cadence ≈6.2–7.2 ms/command at the 1 MiB-per-command grant ⇒
~160 MB/s feed ceiling. The 1 MiB grant matches the smartpqi (Smart Array
P408e-p/E208e-p) max-transfer-per-request ceiling; sg on this host: def_reserved
32 KiB, scatter_elem 32 KiB, allow_dio=0.

**Independent corroboration from the org's own record** (mined email threads,
`~/proposal/research/evidence-problems-waste.md`): IT's weeks of 2025 testing
measured Miria-mediated LTO-9 at 40–299 write / 67–127 read MB/s; a single-tar
360 GB restore at **196 MB/s ("exactly half" of their 400 expectation)**; real
Miria jobs at 167–223 MB/s; their suspicion (4-way SAS splitter cable) was never
validated — and doesn't hold (each SFF-8644 lane = 12 Gb/s ≈ 1.2 GB/s). Their own
conclusion later shifted to the P408e-p controller. Three independent stacks —
Miria, raw tar restore, remanence — converge in the same ~160–220 MB/s band on
this controller family: **platform ceiling, not software.**

**Remedy**: dedicated SAS HBA in place of the RAID-family controller for the tape
path. Recommended: **Broadcom 9500-16e** (SAS3816, PCIe 4.0 x8, mpt3sas, ~16 MiB
max request ⇒ 4 MiB batched commands ⇒ feed capacity ~600 MB/s = drive-limited;
4 external ports = drive scale-out headroom for migration). ~₹40–60k. Diligence:
MSL3040 supported-HBA matrix; HPE-branded Broadcom equivalent if procurement
prefers; existing SFF-8644 cabling reusable. Note drive height: drishti inventory
records HH LTO-9 (native ~300); IT's 400 expectation assumed FH spec — either
way the observed cap sits far below both.

Morning batch sweep (batch 1/2/4 at fixed bytes) remains the final confirmation:
flat ms/command ⇒ HBA-bound (expected); sub-linear growth ⇒ daemon slack too.

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
