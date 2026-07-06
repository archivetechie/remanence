# Panel report -- LTO-9 media readiness

**Date:** 2026-07-06
**Design:** `docs/lto9-media-readiness-design-v0.1.md`
**Status:** folded into design v0.2; verify round pending.
**Panel:** SCSI correctness, failure-modes/ops, fieldtest/operator UX,
cost/efficiency, and GLM 5.2 via OpenRouter (`z-ai/glm-5.2`).

## Verdict

The panel agrees with the core detector: `TEST UNIT READY` returning
`02/04/01` is the portable drive-path signal for "becoming ready", and in an
LTO-9 context it should be treated as media initialization/calibration. The
initial draft was not ready to implement because it treated readiness as a
simple wait loop. Physical LTO-9 handling needs durable fences, selected-library
ownership, phase-split loading, and enforceable quarantine.

Raw findings: 34 total.

- 4 blockers
- 20 majors
- 8 minors
- 2 nits

All blockers and accepted majors were folded into design v0.2.

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

## Verify-Round Focus

The verify review should focus on:

- whether the phase-split load model is implementable without surprising the
  existing Layer 2 semantics;
- whether the robot-move block during active calibration is too conservative or
  correctly cautious for MSL3040 production coexistence;
- whether the durable fence can start as fieldtest-local JSONL or must be
  SQLite from MR-1;
- whether every current drive CDB is covered by the readiness allowlist tests;
- whether the CLI exit-code contract is sufficient for copy-paste-free field
  operation.
