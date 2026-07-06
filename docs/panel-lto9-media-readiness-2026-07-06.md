# Panel report -- LTO-9 media readiness

**Date:** 2026-07-06
**Design:** `docs/lto9-media-readiness-design-v0.1.md`
**Status:** folded into design v0.4; verify round pending.
**Panel:** SCSI correctness, failure-modes/ops, fieldtest/operator UX,
cost/efficiency, and GLM 5.2 via OpenRouter (`z-ai/glm-5.2`).
**Opus addendum:** local Claude Code Opus review run 2026-07-06; 10 findings
folded after the original panel.
**Fable 5 addendum:** Claude Fable 5 via OpenRouter run 2026-07-06; no
blockers, 12 findings folded after Opus.

## Verdict

The panel agrees with the core detector: `TEST UNIT READY` returning the
`02/04/xx` becoming-ready family is the portable drive-path signal, with
`02/04/01` as the documented expected case. In an LTO-9 context it should be
treated as media initialization/calibration unless a known terminal ASCQ
overrides that classification. The
initial draft was not ready to implement because it treated readiness as a
simple wait loop. Physical LTO-9 handling needs durable fences, selected-library
ownership, phase-split loading, and enforceable quarantine.

Raw findings: 34 total.

- 4 blockers
- 20 majors
- 8 minors
- 2 nits

All blockers and accepted majors were folded into design v0.4.

## Blockers Folded

1. **Hidden load composition.** The draft said "wait after load", but current
   `LibraryHandle::load()` already performs `MOVE MEDIUM` plus drive
   `LOAD/UNLOAD` before any TUR wait. The design now requires a phase-split
   pre-ready load path and forbids hiding the readiness path behind the existing
   composed primitive.

2. **No durable long-wait fence.** A 2-hour media-initialization wait cannot
   live only inside a foreground command. Ctrl-C, a terminal disconnect, daemon
   restart, or host reboot could erase the state and allow unsafe retry. The
   design now requires a durable readiness operation record and startup
   reconciliation.

3. **Selected-library/D2 ownership not normative.** The initial CLI/fieldtest
   flow could resolve by barcode or element without explicitly making the
   chosen logical library the security boundary. The design now requires every
   path to carry `--library <serial>` and `--allow <serial>`, refuse
   cross-library barcode resolution, and avoid D2/LTO-7 production partitions.

4. **Stranded calibration state undefined.** `09-media-ready.sh` could load a
   tape, see `media_initializing`, and then continue as if it could leave a
   settled state. The design now says calibration leaves the tape in place,
   records a wait/quarantine ledger entry, prints "do not move/unload/retry",
   and resumes later.

## Major Decisions

- **Short vs long waits.** Daemon/session opens use a short readiness probe.
  The 2-hour wait is explicit media conditioning (`rem tape wait-ready --wait`,
  `rem tape init`, fieldtest readiness).

- **Per-state CDB allowlist.** During `media_initializing`, the affected drive
  permits TUR, limited REQUEST SENSE, and tightly scoped identity probes only.
  All positioning, config, health, read, write, filemark, and load/unload CDBs
  are denied until ready.

- **Completion unknown is cross-cutting.** SG_IO transport failure or timeout
  on any CDB, not only TUR, records opcode, target, timeout class, and dirty
  scope, then routes through quarantine.

- **Unit Attention policy is explicit.** The wait loop may consume one
  reset-class UA per load epoch. Medium-changed and repeated reset UAs require
  inventory/session restart or terminal dirty evidence.

- **Quarantine is enforceable.** Tape, drive, operation/session, and
  selected-library snapshot quarantine records must be admission controls for
  later init/write/read/move/unload paths.

- **Fieldtest keeps existing status vocabulary.** `records.jsonl.status` stays
  `PASS|FAIL|SKIP|INFO`; readiness is recorded separately as
  `media_readiness_state`.

- **CLI is script-stable.** `wait-ready --json` emits one JSON object on stdout,
  progress on stderr, and stable exit codes:
  `0 ready`, `10 initializing`, `20 timeout_unknown`, `30 terminal_error`,
  `40 transport_unknown`, `50 ownership/refused`.

- **READ ELEMENT STATUS ASC/ASCQ is secondary.** Low-level parser/evidence
  support is useful, but daemon/proto exposure is deferred until physical MSL3040
  captures prove it changes decisions.

- **RDY-01 coverage must exist first.** `docs/chaos-adapter-design.md` names
  RDY-01, but current chaos code only defaults RDY-02. The design now requires
  implementing RDY-01 before relying on it for scenarios.

## GLM 5.2 Notes

GLM independently flagged three useful issues:

- `INQUIRY` and `REQUEST SENSE` should not be categorically forbidden during
  not-ready handling. Fold: the design now allows limited identity probes and
  REQUEST SENSE where needed, but still denies data/config/mechanical CDBs.
- A barcode-oriented `wait-ready` command is ambiguous unless scoped to a
  selected library and loaded-drive mapping. Fold: barcode mode is selected
  library only; element mode is explicit/operator-only.
- `host_status=0x0003` should be named as `DID_TIME_OUT`, not vaguely described.
  Fold: the design now records the Linux host status while preserving the
  operation-level completion-unknown safety model.

## Opus Addendum

Opus did not participate in the original committed panel. A local Claude Code
Opus review was run afterward at the owner's request. It found one remaining
blocker and several major/minor precision gaps; all accepted findings were
folded into design v0.3.

Key folds:

- **TUR before explicit drive LOAD.** The v0.2 design still allowed
  unconditional pre-ready drive `LOAD` (`0x1b`) after `MOVE MEDIUM`, but the
  July incident included task aborts on opcode `0x1b`. v0.3 now probes TUR
  after `MOVE MEDIUM` and skips explicit `LOAD` if the drive is already
  becoming ready. `LOAD` is conditional, fenced, and used only for states such
  as `02/04/02`.
- **Broader becoming-ready classifier.** v0.2 matched only `02/04/01`. v0.3
  treats the `02/04/xx` becoming-ready family as non-terminal during an
  LTO-9/LZ media-conditioning epoch, preserving exact ASCQ in evidence.
- **Volatile sg paths.** v0.3 requires startup reconciliation to re-resolve
  changer/drive sg paths from `library_serial` and `drive_serial` before any
  TUR. Stored `/dev/sg*` paths are hints, never authority.
- **`06/28/00` inside readiness.** v0.2 treated `28/00` as requiring inventory
  and session restart. v0.3 treats one `28/00` inside the same
  media-conditioning epoch as the expected not-ready-to-ready transition, then
  confirms with TUR GOOD.
- **Timeout and resume.** The long-wait default is now 2.5h rather than 2h, and
  `rem tape wait-ready --resume <operation_id>` is part of the CLI contract.
- **SQLite authority.** The durable fence is now SQLite from MR-1; JSONL is
  evidence/export only.

## Fable 5 Addendum

Claude Fable 5 was run through OpenRouter after the Opus fold. It found no
remaining blockers, but it did find four majors and several smaller precision
issues. Accepted findings were folded into design v0.4.

Key folds:

- **Immediate conditional LOAD.** If readiness handling ever issues conditional
  drive `LOAD` (`0x1b`), it must use `IMMED=1` and then rely on TUR polling.
  Non-immediate LOAD is too likely to recreate the July `DID_TIME_OUT` /
  `smartpqi` abort pattern.
- **Terminal `02/04/xx` exceptions.** The design now treats known
  operator-intervention/reset-required ASCQs, starting with `04/03` and
  `04/20..22`, as terminal immediately rather than waiting 2.5h under a
  misleading `media_initializing` label.
- **Target-status classification.** BUSY, RESERVATION CONFLICT, TASK SET FULL,
  and TASK ABORTED are not transport failures. The design now classifies them
  separately, with bounded retry only for BUSY/TASK SET FULL and terminal
  ownership refusal for RESERVATION CONFLICT.
- **Quarantine release CLI.** The design now specifies
  `rem tape quarantine list/show/release`; fences are no longer enforceable but
  unreleasable.
- **TUR/REQUEST SENSE are UA-consuming.** Non-selected libraries and D2/LTO-7
  devices may be probed only with INQUIRY/VPD from readiness code.
- **Authoritative state for scripts.** Fieldtest scripts must query the
  SQLite-backed CLI surface; JSONL remains evidence only.

## Verify-Round Focus

The verify review should focus on:

- whether the phase-split load model is implementable without surprising the
  existing Layer 2 semantics;
- whether the robot-move block during active calibration is too conservative or
  correctly cautious for MSL3040 production coexistence;
- whether the SQLite MR-1 fence is minimal enough while still being a real
  admission-control store;
- whether every current drive CDB is covered by the readiness allowlist tests;
- whether the CLI exit-code and quarantine-release contracts are sufficient for
  copy-paste-free field operation.
