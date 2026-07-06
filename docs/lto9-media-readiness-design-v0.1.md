# LTO-9 media readiness and calibration-aware orchestration -- Design v0.3

**Status:** panel folded + Opus review folded (2026-07-06); verify round
pending.
**Panel 2026-07-06:** 34 raw findings across SCSI correctness,
failure-modes/ops, fieldtest/operator UX, cost/efficiency, and GLM 5.2 via
OpenRouter (`z-ai/glm-5.2`). Raw findings: 4 blockers, 20 majors, 8 minors,
2 nits. Folded decisions are recorded in
`docs/panel-lto9-media-readiness-2026-07-06.md`.
**Opus addendum 2026-07-06:** local Claude Code Opus review found 10 findings
(1 blocker, 3 majors, 4 minors, 2 nits); all accepted findings are folded in
this revision.
**Problem source:** the July 2026 physical MSL3040 field test exposed that
`rem tape init` and `fieldtest/scripts/10-init-pools.sh` are not aware of
LTO-9 first-load media optimization. `AOX034L9` initialized successfully, but
`AOX030L9` entered a long library-reported `Calib` state. Follow-on host
commands then saw Unit Attention, SG_IO transport timeout/completion-unknown
errors, and `smartpqi` task abort/reset lines. The software treated this like
ordinary init failure and destructive escalation, not like an expected
media-readiness state.
**External references:** Oracle documents LTO-9 calibration returning
`NOT READY / BECOMING READY` to data-path SCSI commands:
<https://docs.oracle.com/en/storage/tape-storage/sl4000/slksr/behavior-ibm-lto9-tape-calibration.html>.
IBM documents first-load L9/LZ media optimization, its 40-minute average,
possible 2-hour duration, and the "do not interrupt" rule:
<https://www.ibm.com/docs/en/ts4500-tape-library?topic=performance-media-optimization>.
The T10 ASC/ASCQ list defines `04h/01h` as "logical unit is in process of
becoming ready": <https://www.t10.org/lists/asc-num.htm>. LTO.org documents
that LTO-9 media initialization is one-time for new LTO-9 cartridges:
<https://www.lto.org/faqs-about-lto/>.
**Local evidence:** `~/remfield/evidence/10-init-pools/20260706T101147Z-init-AOX030L9.json`,
`~/remfield/evidence/manual-selection/20260706T104106Z-AOX030L9-retry-force.txt`,
and the RCA note in `~/drishti/docs/msl3040-remfield-rca-notes.md`.
**Precedent docs:** `docs/chaos-adapter-design.md` RDY-01,
`docs/chaos-phase-e-changer-faults-design-v0.1.md`,
`docs/drive-stewardship-design-v0.1.md`,
`docs/layer5-multi-object-append-design-v0.1.md`,
`fieldtest/RUNBOOK.md`.

---

## 1. Executive decision

Remanence must add an explicit, persisted media-readiness state machine between
"we intend to put this cartridge into a drive" and "the drive is safe for BOT
reads, configuration reads, rewinds, writes, unloads, or destructive retry
escalation."

The core detector is `TEST UNIT READY`:

- `GOOD` means the drive is ready for normal Remanence commands.
- sense key `0x02`, ASC `0x04`, ASCQ in the becoming-ready family means the
  logical unit is becoming ready. In an LTO-9/LZ context, Remanence displays
  this as `media_initializing` and treats it as the expected first-load
  optimization window.
- the same `02/04/xx` family outside LTO-9 context stays generic
  `becoming_ready`; it is not globally labeled "calibration."
- exact ASCQ is preserved in evidence. `04/01` is the documented expected
  signal; `04/00` and `04/07` remain non-terminal becoming-ready states during
  a media-conditioning epoch; `04/02` routes to the conditional explicit-load
  branch.

This gate is not an `mtx` problem and not an HPE UI scrape. Remanence uses raw
SCSI already, so it can see the same TUR sense and SG_IO transport failures
directly. Vendor UI state such as `Calib` is corroborating evidence only.

The fold changes the initial draft in three important ways:

1. The readiness operation is **durable**. A 2-hour wait cannot live only in a
   foreground shell process or tmux pane.
2. The wait is **selected-library scoped**. Every path carries the chosen
   library serial and static allowlist; barcode resolution never crosses into
   the D2/LTO-7 partition.
3. The load path is **phase-split**. The existing `LibraryHandle::load()` is a
   composed `MOVE MEDIUM` + drive `LOAD`. The readiness path must not hide that
   composition behind a function named "load after which we wait."
4. Explicit drive `LOAD` (`0x1b`) is **conditional**, not automatic. After
   `MOVE MEDIUM`, Remanence probes TUR first and skips drive `LOAD` if the
   drive is already loading/threading or becoming ready.

## 2. Field facts

Known-good rem-side mapping after rescan:

```text
DEC418146K_LL02
  changer: /dev/sg8
  drive 0x0001: /dev/sg7   LTO-9 serial 8031BDC7D1
  drive 0x0002: /dev/sg11  LTO-9 serial 8031BDC7DB
```

The same host also exposed D2/LTO-7 devices. Those are production restore
devices and are out of scope for rem field tests.

Observed sequence:

1. `AOX034L9` initialized successfully through `10-init-pools.sh`.
2. `AOX030L9` failed during `tape init` after being loaded into drive bay
   `0x0001`.
3. The first failure surfaced as Unit Attention `29/00` while reading drive
   config.
4. A targeted retry failed on `REWIND` with SG_IO `host_status=0x0003`
   (`DID_TIME_OUT`). For this workflow that is still completion-unknown: the
   host timed out, while the drive/library may have continued mechanical work.
5. Kernel logs showed `smartpqi` task aborts/resets for opcodes `0x1b`
   (`LOAD/UNLOAD`) and `0x01` (`REWIND`) on the LTO-9 drive target.
6. The MSL3040 web UI showed `Calib`, later `Unload`, for the same cartridge.
7. A later changer status command blocked in `sg_ioctl_common`, and the kernel
   reset the changer target for opcode `0xa5` (`MOVE MEDIUM`).

Conclusion: the incident does not prove that the individual CDBs are invalid.
It proves the orchestration issued normal init/retry/unload/status commands
into a period where the cartridge, drive, and library were not settled.

## 3. Current software gap

`DriveHandle::test_unit_ready()` exists in
`crates/remanence-library/src/handle/tape_io/mod.rs`, but today it is a simple
heartbeat. It returns only `Ok(())` or a mapped `TapeIoError`; there is no
public media-readiness classifier.

`rem tape init` currently does this after a slot target is resolved:

1. `LibraryHandle::load(slot, bay)`; today that is `MOVE MEDIUM` followed by
   drive `LOAD` (`0x1b`);
2. open drive;
3. `read_config()` (`READ BLOCK LIMITS` then `MODE SENSE`);
4. check write protect/WORM;
5. `rewind()`;
6. read BOT/classify;
7. maybe write bootstrap.

For a newly loaded LTO-9 cartridge still calibrating, steps 3 and 5 are too
aggressive. The more subtle bug is step 1: the readiness design cannot say
"wait after load" while using a composed load primitive that already issued a
drive mechanical command.

`fieldtest/scripts/10-init-pools.sh` currently retries:

```text
plain -> --force -> --clobber-data
```

That is correct for stale Remanence identity or deliberately destructive
scratch reuse. It is wrong for media initialization. If the failure is
`media_initializing`, the script must stop the destructive escalation ladder
and move into a wait/quarantine workflow.

## 4. Goals

- Recognize LTO-9 first-load media initialization without requiring the HPE web
  UI or `mtx`.
- Keep Remanence safe on physical libraries by avoiding disruptive follow-on
  drive/changer commands while media initialization is active.
- Make `rem tape init` and fieldtest scripts deterministic: `ready`,
  `becoming_ready`, `media_initializing`, `timeout_unknown`,
  `terminal_error`, and `transport_unknown` are distinct outcomes.
- Preserve strict safety for destructive init escalation. `--force` and
  `--clobber-data` must not be retried because a cartridge is merely becoming
  ready.
- Persist readiness fences so Ctrl-C, terminal loss, daemon restart, or a host
  reboot does not erase an in-progress physical state.
- Emit structured evidence usable by Drishti and the fieldtest evidence bundle.
- Keep VTL and existing scenario behavior fast by separating short readiness
  probes from explicit long media-conditioning waits.

## 5. Non-goals

- Scraping the MSL3040 web UI.
- Depending on `mtx` output.
- Vendor-locking the core detector to HPE. HPE-specific management status can
  be a later optional adapter, not the base rule.
- Making `04/01` mean "LTO-9 calibration" everywhere.
- Automatically unloading or moving a cartridge after transport
  unknown-completion. Recovery is a separate operator action after a settled
  inventory.
- Public daemon/API exposure of READ ELEMENT STATUS descriptor ASC/ASCQ in
  this milestone. The first cut may keep it in CLI/evidence only.

## 6. Durable readiness operation

Introduce a durable readiness operation record before any phase that may put a
cartridge into a drive or wait for it:

```text
media_readiness_ops
  operation_id
  run_id
  library_serial
  changer_sg
  drive_element
  drive_sg
  drive_serial
  barcode
  source_slot
  media_generation
  phase
  state
  dirty_scope
  started_at_utc
  updated_at_utc
  last_cdb_opcode
  last_sense_raw
  last_sense_key
  last_asc
  last_ascq
  last_host_status
  last_driver_status
  cancel_source
  signal
  evidence_path
```

The backing store is SQLite from MR-1. JSONL remains an evidence/export format
for fieldtest and Drishti, not the authoritative admission-control store.

Durable rules:

1. Write `planned` before `MOVE MEDIUM` or any pre-ready drive `LOAD`.
2. Write `pre_ready_loading` before each mechanical pre-ready phase.
3. Write every readiness transition (`becoming_ready`, `ready`,
   `timeout_unknown`, `transport_unknown`, `terminal_error`).
4. On Ctrl-C/SIGTERM after pre-ready loading began, mark
   `aborted_unknown`; do not issue cleanup CDBs from the signal path.
5. On daemon/CLI startup, reconcile open records before admitting new load,
   init, unload, move, write, or read-session operations for the same scope.

Startup reconciliation never trusts stored `/dev/sg*` paths by themselves.
They are volatile after reboot, rescan, or recabling. Reconciliation first
re-resolves the selected library and drive from durable identity:

- `library_serial` -> current changer sg path via discovery/INQUIRY identity;
- `drive_serial` + `library_serial` + `drive_element` -> current drive sg path;
- barcode -> current selected-library inventory binding.

Only after serial identity is reverified may the reconciler issue a read-only
TUR. It must never issue MOVE or LOAD as part of startup reconciliation. If the
serial binding is missing, ambiguous, or points outside the selected rem
library, the record remains fenced and operator/RCA recovery is required.

This is the main safety fold from the panel. A long physical operation must
remain visible after the shell that started it disappears.

## 7. Phase-split load and readiness state machine

The readiness path must not call the existing composed `LibraryHandle::load()`
until that method is refactored or wrapped with explicit phase reporting.

Required phases:

1. **Resolve in selected library.** Find the barcode only within the selected
   `library_serial`, with `--allow <serial>` applied. If the barcode is absent
   or ambiguous, fail before issuing any CDB.
2. **Pre-ready mechanical placement.** If the tape is in a slot, first issue
   only `move_medium(slot, bay)` and durably record completion or
   completion-unknown. Then open the drive by verified serial and issue TUR.
   If TUR returns GOOD or a becoming-ready `02/04/xx` sense, skip explicit
   drive `LOAD` and enter readiness polling. Issue drive `LOAD` (`0x1b`) only
   when TUR indicates no medium or an initializing-command-required state
   such as `02/04/02` after a short settle. The drive `LOAD` exception is
   therefore conditional and evidence-backed, not automatic.
3. **Readiness polling.** After the pre-ready phase, issue only allowed
   readiness-state commands until the drive reaches `Ready` or a terminal
   state.
4. **Normal init/session setup.** Only after `Ready` may `read_config`,
   `rewind`, BOT read, write, read, locate, or health polling proceed.

State vocabulary:

```rust
enum MediaReadiness {
    Ready,
    BecomingReady {
        sense_key: u8, // 0x02
        asc: u8,       // 0x04
        ascq: u8,      // exact ASCQ: 0x00/0x01/0x02/0x07/etc.
        display: BecomingReadyDisplay, // MediaInitializing for LTO-9/LZ
        observed_for: Duration,
    },
    UnitAttention {
        asc: u8,
        ascq: u8,
    },
    NoMedium,
    TerminalNotReady {
        sense_key: u8,
        asc: u8,
        ascq: u8,
    },
    TransportUnknown {
        opcode: u8,
        status: u8,
        host_status: u16,
        driver_status: u16,
        info: u32,
        dirty_scope: DirtyScope,
    },
}
```

`BecomingReadyDisplay::MediaInitializing` is used when the cartridge is LTO-9
or LZ and the state follows a load/already-loaded LTO-9 context. Before the
drive is ready, the generation source is the selected-library barcode suffix
and allowlist metadata (`L9`/`LZ`), not MODE SENSE. The host does not need to
prove first-load; the sense family says the drive is not ready, and the LTO-9
context explains the likely cause.

## 8. Command policy while initializing

Use a per-state allowlist, not a partial deny list.

Allowed against the affected drive while `becoming_ready/media_initializing`:

- `TEST UNIT READY`;
- `REQUEST SENSE` only if explicitly needed after a CHECK CONDITION path not
  already carrying sense bytes;
- `INQUIRY`/VPD identity probes only before the wait starts or after `Ready`,
  unless an implementation needs one read-only identity check to bind the
  durable record to a drive serial.

Denied against the affected drive until `Ready`:

- `READ BLOCK LIMITS`;
- `MODE SENSE`;
- `MODE SELECT`;
- `REWIND`;
- `LOCATE`;
- `SPACE`;
- `READ POSITION`;
- `READ`;
- `WRITE`;
- `WRITE FILEMARKS`;
- `LOG SENSE` health/TapeAlert/error-counter reads;
- `LOAD/UNLOAD` retries;
- direct dev write/read-dump helper paths.

Library inventory (`READ ELEMENT STATUS`) is conditionally allowed only when no
completion-unknown signal has occurred for the changer/logical library. It must
use bounded timeout/backoff and the selected library serial. If inventory itself
times out or triggers a transport reset, the logical library snapshot becomes
dirty and must not be used as harmless corroboration.

Robot moves (`MOVE MEDIUM`) within the same logical library are blocked while
there is an active `media_initializing` fence, unless an explicit
operator-approved future `parallel-conditioning` mode proves safe on that
library. This conservative default matches the July incident and protects the
central-IT production window.

Foreign/D2 devices are always excluded: no readiness command path may issue a
state-changing CDB outside the selected rem library.

## 9. Unit Attention and transport policy

The wait loop may consume at most one reset-class Unit Attention per load epoch:

- `06/29/00` power on, reset, or bus device reset occurred;
- `06/29/01..04` reset-family variants if the decoded ASCQ is explicitly
  added to the implementation table with a test.

After that single reset-class UA, wait a short HBA/library settle delay and
issue another TUR. The re-probe clears the UA; the delay is for practical
transport recovery, not for SCSI UA clearing itself.

UA handling differs inside and outside the active media-conditioning epoch.

Inside an active media-conditioning epoch for the same drive/barcode,
`06/28/00` is treated as an expected not-ready-to-ready transition: consume it
once, wait a short settle delay, and confirm with TUR GOOD. Do not immediately
turn that positive transition into a changer inventory operation.

Do not swallow these as harmless readiness:

- `06/28/00` outside the active readiness epoch, or when barcode/drive binding
  no longer holds: requires fresh selected-library inventory and session
  restart.
- mode-parameter changed UAs: require a fresh `read_config()` after `Ready`.
- repeated reset UAs within the same load epoch: become terminal dirty evidence.

Completion unknown is cross-cutting, not just a readiness helper state. Any CDB
that returns SG_IO transport failure, timeout, or reset evidence records:

- opcode and operation name;
- target sg path and H:C:T:L if known;
- timeout class;
- drive/library/barcode context;
- dirty scope.

Dirty scope defaults:

| Failing command | Default dirty scope |
|---|---|
| drive TUR/read_config/rewind/read/write/locate/space/filemark | affected drive + tape/session |
| drive `LOAD/UNLOAD` | affected drive + tape/session; library if called from a composed move/load/unload |
| changer `MOVE MEDIUM` | selected logical library snapshot + both endpoints |
| changer `READ ELEMENT STATUS` timeout/reset | selected logical library snapshot |

`host_status=0x0003` is recorded as `DID_TIME_OUT`; Remanence still treats the
operation result as completion-unknown for media-position safety.

## 10. Wait profiles

There are two wait profiles:

| Profile | Default use | Timeout | Behavior |
|---|---|---:|---|
| `short_probe` | daemon write/read/session opens, normal already-optimized media | 30-60s | fail fast with `becoming_ready`/`media_initializing` and recommend explicit wait |
| `media_conditioning` | `rem tape init`, `rem tape wait-ready --wait`, fieldtest readiness phase | 2.5h | durable long wait; no destructive escalation; operator-visible evidence |

This prevents normal daemon operations from silently occupying a physical drive
for hours. Long waits are explicit physical-media conditioning operations. The
2.5-hour default is intentionally above the documented "up to 2 hours" media
optimization window so a normally completing cartridge is not fenced exactly at
the boundary.

Default physical LTO-9 `media_conditioning` polling:

- 15 seconds for the first minute;
- 60 seconds steady-state;
- optional small jitter when a future parallel-conditioning mode is enabled.

Timeout is fail-closed. `timeout_unknown` keeps the tape/drive/session fenced
and forbids `--force`/`--clobber-data` until a recovery command records settled
inventory and operator/RCA acknowledgment.

## 11. CLI contract

Add:

```text
rem tape wait-ready --library <serial> --barcode <barcode> [--wait] [--timeout 2.5h] [--json]
rem tape wait-ready --library <serial> --drive-element <0xNNNN> --already-loaded --json
rem tape wait-ready --resume <operation_id> [--json]
```

Barcode mode is the normal mode. It resolves only inside the selected library
and only for allowlisted rem operations. It may perform pre-ready mechanical
placement if the command is in `--wait` mode and the barcode is allowlisted.

Drive-element mode is operator-only/readiness-only. It never searches other
libraries and is disallowed in fieldtest scripts unless the element is tied to
an allowlisted barcode in the selected-library snapshot.

`--resume` attaches to the existing durable readiness operation instead of
re-planning the load. Resume-by-barcode is allowed only when the selected
library and barcode still bind to exactly one fenced operation.

Exit codes:

| Code | Meaning |
|---:|---|
| 0 | ready |
| 10 | still initializing/becoming ready; wait can be resumed |
| 20 | timeout_unknown; fenced |
| 30 | terminal_error; JSON `operator_action` is required to distinguish fenced hardware/media error from policy refusal |
| 40 | transport_unknown; fenced, RCA required |
| 50 | ownership/refused: wrong library, absent barcode, ambiguous barcode, or allowlist failure |

With `--json`, stdout is one JSON object. Human progress goes to stderr.
Script-facing JSON includes `recommended_next_command` and `operator_action`.

`rem tape init` calls the same helper in `media_conditioning` mode for physical
LTO-9/LZ. If the helper exits 10/20/40, init stops before `read_config`,
`rewind`, `--force`, or `--clobber-data`.

## 12. Fieldtest contract

Add a fieldtest readiness phase:

```text
09-media-ready.sh --count N [--condition-all] [--resume]
```

Default behavior:

1. Resolve the selected library from `state/selected-library` or
   `FIELDTEST_LIBRARY_SERIAL`.
2. Use only allowlisted scratch data barcodes visible in that exact library.
3. Process only the tapes required by `--count`, unless `--condition-all` is
   explicitly passed.
4. On `media_initializing`, leave the tape in place, record a durable
   wait/quarantine record, print "do not move/unload/retry", and exit 10.
5. On `ready`, record a readiness ledger entry consumed by `10-init-pools.sh`.
6. On `transport_unknown` or `timeout_unknown`, stop and request RCA.

`10-init-pools.sh` then consumes `state/media-readiness.jsonl`:

- if a barcode is already `ready`, proceed with init;
- if a barcode is `media_initializing`, record `INFO` with
  `media_readiness_state=media_initializing`, do not force/clobber, and either
  continue with another ready barcode or exit 10 when the selected run path no
  longer has enough ready media;
- if a barcode is quarantined, skip it and explain the release requirement;
- destructive escalation only runs after readiness is `ready` and only for
  actual tape-init policy refusals.

`records.jsonl.status` remains within the existing fieldtest vocabulary:
`PASS`, `FAIL`, `SKIP`, `INFO`. Media readiness is a separate field:
`media_readiness_state`.

The safe default does not use the robot while a calibration fence is active. A
future `--parallel-conditioning` mode may initialize multiple new cartridges up
to available drives, but only after the implementation proves the selected
library supports that safely and still preserves the D2/foreign boundary.

## 13. Quarantine and release

Quarantine records are enforceable admission controls, not prose:

- tape/barcode quarantine;
- drive-bay quarantine;
- readiness operation/session quarantine;
- selected-library snapshot dirty state.

Selection, init, write, read, move, unload, and fieldtest scripts must consult
these records before acting.

Release criteria:

- `media_initializing` -> `ready`: TUR GOOD plus selected-library snapshot that
  still binds the barcode to the expected drive/slot.
- `timeout_unknown`: operator recovery command after settled inventory; no
  force/clobber shortcut.
- `transport_unknown` on a drive command: settled inventory for that drive/tape,
  no active kernel reset loop, operator/RCA acknowledgment.
- `transport_unknown` on changer `MOVE MEDIUM` or RES: selected logical library
  inventory must be trusted again before any robot move. This can require both
  DTEs/endpoints to be reconciled, but "both drives empty" is not the universal
  release condition.

## 14. READ ELEMENT STATUS secondary signal

READ ELEMENT STATUS element descriptor bytes 4 and 5 contain ASC/ASCQ.
Remanence already parses adjacent `EXCEPT` and `ACCESS` bits.

For this milestone:

- parse and retain descriptor ASC/ASCQ in the low-level SCSI `Element` type if
  implementation cost is small;
- expose it in direct CLI JSON/evidence when `except` is set;
- do not add daemon proto/API fields until a physical MSL3040 capture proves
  this changes an operator decision.

This signal answers: "Does the changer also know this element is abnormal?" It
does not replace TUR, because calibration readiness is a drive data-path state
and vendor libraries differ in what they surface through element status.

## 15. Observability

Every readiness transition emits flattened fields compatible with the Drishti
RCA note:

```text
component=remanence
subsystem=media_readiness
timestamp_utc=...
run_id=...
session_id=...
operation_id=...
attempt_id=...
script=...
test_id=...
command_argv=...
exit_code=...
library_serial=...
library_allowed=true|false
changer_sg=...
drive_element=...
drive_sg=...
drive_hctl=...
drive_serial=...
source_slot=...
barcode=...
media_generation=LTO-9
state=becoming_ready|media_initializing|ready|timeout_unknown|transport_unknown|terminal_error
sense_raw=...
sense_key=...
asc=...
ascq=...
host_status=...
driver_status=...
cdb_opcode=...
last_cdb_opcode=...
dirty_scope=...
quarantine_id=...
cancel_source=...
signal=...
poll_count=...
elapsed_ms=...
library_snapshot_path=...
kernel_log_window_path=...
evidence_path=...
recommended_next_command=...
operator_action=...
```

This is intentionally broader than the initial draft. Drishti needs enough
context to correlate Remanence evidence, kernel `smartpqi` lines, process
interrupts, and selected-library topology.

## 16. Testing and coverage

Unit coverage:

- sense classifier tests for fixed and descriptor sense:
  `02/04/00`, `02/04/01`, `02/04/02`, `02/04/07`, `06/29/00`, `02/3A/00`,
  `06/28/00`, representative terminal errors;
- `host_status=0x0003` is labeled `DID_TIME_OUT` and still maps to
  completion-unknown for media-position safety;
- wait algorithm with fake clock: N `02/04/xx` polls then GOOD, timeout,
  one allowed reset UA then GOOD, repeated reset UA dirty, transport unknown
  fail-closed;
- pre-ready load path does not call the old hidden composed `LibraryHandle::load`
  without phase evidence;
- after `MOVE MEDIUM`, TUR `02/04/01` skips explicit drive `LOAD`; TUR
  `02/04/02` may issue one fenced conditional drive `LOAD`;
- generation/display mapping uses barcode suffix/allowlist metadata before
  readiness, including `LZ` handling;
- READ ELEMENT STATUS parser tests retain descriptor ASC/ASCQ if MR includes
  that low-level addition.

Chaos/harness coverage:

- implement RDY-01 in `remanence-chaos` before relying on it:
  `SK=02 ASC=04 ASCQ=01` on TUR, time-scaled for CI;
- assert that while RDY-01 is active, all non-allowlisted drive CDBs are refused
  or not issued: `READ BLOCK LIMITS`, `MODE SENSE`, `MODE SELECT`, `REWIND`,
  `LOCATE`, `SPACE`, `READ POSITION`, `READ`, `WRITE`, `WRITE FILEMARKS`,
  `LOG SENSE`, `LOAD/UNLOAD`;
- add a scenario or `covers` entry proving `rem tape init` stops before
  destructive escalation on `media_initializing`;
- add a two-logical-library fixture proving an allowlisted-looking barcode
  outside the selected library is refused without load/move/TUR to that library;
- add fieldtest dry-run coverage for `records.jsonl.status` plus
  `media_readiness_state`.

Physical coverage:

- one controlled MSL3040 run on a known-new LTO-9 cartridge, capturing:
  `rem tape wait-ready --json`, selected library slots before/after, kernel
  dmesg filter, and fieldtest evidence;
- confirm D2/LTO-7 devices receive no state-changing CDBs during the run.

## 17. Rollout

1. **MR-1 classifier + durable operation record.** Add media-readiness
   classifier, dirty-scope model, and SQLite-backed authoritative store
   (JSONL export only).
2. **MR-2 phase-split load primitive.** Expose or implement pre-ready load
   phases without using the hidden composed `LibraryHandle::load()` path;
   after `MOVE MEDIUM`, TUR before any conditional drive `LOAD`.
3. **MR-3 `rem tape wait-ready`.** Operator-visible CLI, JSON, exit codes,
   selected-library ownership checks.
4. **MR-4 init integration.** Gate `rem tape init` before `read_config()`;
   block destructive escalation on readiness states.
5. **MR-5 chaos and scenario coverage.** Implement RDY-01 and command-allowlist
   assertions.
6. **MR-6 fieldtest integration.** Add `09-media-ready.sh`, ledger consumption
   in `10-init-pools.sh`, and evidence summaries.
7. **MR-7 daemon/session integration.** Short-probe profile for physical
   write/read opens; long wait only by explicit operator/media-conditioning
   policy.
8. **MR-8 Drishti alignment.** Ensure logs match the RCA fields already noted
   in `~/drishti/docs/msl3040-remfield-rca-notes.md`.

## 18. Verify-round questions

1. Is the default robot-move block during active calibration too conservative
   for two-drive MSL3040 conditioning, or should parallel conditioning remain a
   future explicit mode?
2. Should READ ELEMENT STATUS ASC/ASCQ be included in MR-1 as low-level CLI
   evidence, or deferred entirely until after the TUR gate ships?
3. Does physical MSL3040 capture show `02/04/01`, `02/04/07`, another
   `02/04/xx`, or only Unit Attention/timeout during LTO-9 media
   initialization? Keep the classifier broad until that capture is known.
