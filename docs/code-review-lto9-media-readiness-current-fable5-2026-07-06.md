# Code review -- LTO-9 media-readiness current gate, Fable 5

**Date:** 2026-07-06
**Reviewer:** Claude Fable 5 via OpenRouter
(`anthropic/claude-fable-5`, routed as `anthropic/claude-5-fable-20260609`)
**OpenRouter ID:** `gen-1783350114-sheEgb09QwyuQHMhpJzI`
**Scope:** current LTO-9 media-readiness/calibration implementation state after
the MSL3040 field incident, including the TUR classifier, direct `tape init`
readiness gate, fieldtest readiness scripts, and RDY-01 chaos coverage.

## Verdict

Fable 5 marked the current state **BLOCKED**: the direct CLI gate is on the
right path, but the fieldtest escalation wrapper and scenario/chaos gates are
not yet strong enough to treat the implementation as fully ready for the next
physical run without caveats.

## Findings

- **High:** `fieldtest/scripts/10-init-pools.sh` escalation is guarded by
  string matching, not by structured status or exit code. Fable's concern is
  that any readiness stop whose output wording changes can fall through to
  `--force` and then `--clobber-data`. Recommended fix: treat readiness-related
  exit codes such as 10/30/40/50 as non-escalating stops, with grep matching
  only as additional evidence.
- **High:** the already-loaded drive path in `run_tape_init_hardware` polls
  readiness directly, while the slot path has the conditional immediate LOAD
  branch. If TUR reports `02/04/02`, an already-loaded cartridge may wait until
  timeout instead of issuing the single fenced conditional load. Recommended
  fix: apply the same conditional-load predicate to the in-drive path.
- **High:** the forbidden-CDB assertion in the existing TUR readiness test only
  checks a subset of the §16 list. Fable called out unasserted commands:
  `MODE SELECT`, `LOCATE`, `SPACE`, `READ`, `WRITE`, `WRITE FILEMARKS`, and
  `LOAD/UNLOAD` outside the fenced phase. Recommended fix: expand the test or
  add scenario coverage for the complete forbidden set.
- **Medium:** repeated Unit Attention is terminal inside one readiness epoch.
  Fable noted that post-reset MSL3040/HBA behavior can produce stacked UAs and
  recommended a bounded drain of distinct UA sense codes before terminalizing.
- **Medium:** RDY-01 chaos coverage is fixed-sense injection rather than a
  native model readiness state machine. Fable judged this insufficient to prove
  BecomingReady-to-Ready progression or the `02/04/02` conditional-load path.
- **Medium, inferred:** the review packet did not include the implementation
  of `readiness_requires_conditional_load`; Fable asked for explicit tests that
  `02/04/01` skips drive LOAD and `02/04/02` may issue exactly one IMMED LOAD.
- **Low:** `TaskAborted` is classified terminal. Fable considered this
  conservative but potentially noisy after host resets.
- **Low:** the fieldtest evidence classification has the same grep fragility
  as the escalation guard and should prefer rc/JSON parsing.

## Coverage Gate Status

- **Closed:** basic classifier tests and the TUR-only readiness probe.
- **Partial:** wait algorithm behavior, phase-split load behavior,
  `02/04/01` versus `02/04/02` behavior, RDY-01 fixed-sense chaos, and direct
  CLI stop-before-init behavior.
- **Open:** complete forbidden-CDB scenario/chaos assertion, a scenario or
  `covers` entry proving `rem tape init` stops before destructive escalation,
  the two-logical-library fixture, and fieldtest dry-run coverage proving
  `records.jsonl.status` plus `media_readiness_state`.

## Field-Test Guidance

Fable's operational recommendation was not to treat the fieldtest wrapper as
fully safe until the init escalation guard is exit-code or JSON based. For an
urgent physical run before those fixes, it recommended disabling the
`--force`/`--clobber-data` escalation steps, treating repeated-UA rc 30 as
suspect rather than conclusive, and accepting that an already-loaded
`02/04/02` cartridge may time out until the in-drive conditional-load branch is
fixed.

## Follow-up Fold

The immediate safety findings from this review were folded in the follow-up
implementation slice:

- `rem tape init` readiness failures now carry stable
  `media_readiness_state=<state>` and `media_readiness_exit_code=<code>`
  metadata and propagate the readiness exit code, including from `--dry-run`.
- `fieldtest/scripts/10-init-pools.sh` now stops destructive escalation from
  rc/metadata before falling back to legacy grep evidence, and records
  `media_readiness_state` plus `rem_exit_code` into `records.jsonl`.
- already-loaded `tape init` candidates now run the same post-TUR conditional
  immediate LOAD logic as slot-loaded candidates.
- the TUR readiness regression checks the full forbidden media/config CDB set
  from the design.
- `remanence-chaos` `ModelTransport` now supports honest TUR for loaded and
  empty drives; RDY-01 fixed-sense injection now layers on a native model CDB.

Remaining gates after that fold: full scenario or `covers` evidence for
destructive-escalation refusal, the two-logical-library fixture, and broader
wait/repeated-UA scenario coverage.

## Coverage Fold

The next implementation slice added local regression coverage for the remaining
software gates:

- `fieldtest/scripts/10-init-pools.sh --selftest` now runs the actual init
  ladder against a fake `rem` that returns readiness exit code 10 on the
  dry-run call, then asserts no `--force` or `--clobber-data` invocation
  occurred and that `records.jsonl` carries `INFO`,
  `media_readiness_state`, and `rem_exit_code`.
- `remanence-cli` now has a two-logical-library resolver fixture proving a
  barcode visible only in a non-selected partition is reported absent from the
  selected library before hardware execution.
- `remanence-cli` now drives the real readiness poll loop through a synthetic
  `DriveHandle` and proves the second reset-class Unit Attention in one epoch
  terminalizes as `RepeatedUnitAttention` with design exit code 30.

Remaining evidence gap: physical MSL3040 capture when a new LTO-9 cartridge is
available.

## Fieldtest Wrapper Fable Pass

Claude Fable 5 was run again through OpenRouter on the fieldtest wrapper
contract (`anthropic/claude-fable-5`, routed as
`anthropic/claude-5-fable-20260609`; OpenRouter IDs
`gen-1783353138-yJF5kKSK5qMECCXfr5tb` and
`gen-1783353213-z4c3biBrENOlOlmOndHu`).

Verdict: **no-go for physical fieldtest until `09-media-ready.sh` matches the
contract.**

Accepted findings:

- `09-media-ready.sh` returned 0 for `media_initializing` exit code 10 and
  invited retry instead of saying not to move/unload/retry.
- The contracted `--count N [--condition-all]` fieldtest surface was absent.
- Single-barcode and drive-element paths did not enforce allowlist and selected
  library visibility before polling.
- The wrapper did not honor `FIELDTEST_LIBRARY_SERIAL` when state selection was
  absent.
- The runbook made 09 only a reactive fallback, and 09 could not bootstrap its
  config.
- `10-init-pools.sh` fallback transport detection matched `host_status=0x00`.

Fold:

- `09-media-ready.sh` now supports count mode, `--condition-all`, env-or-state
  library selection, config bootstrap, allowlist/visibility checks, slot-only
  `SKIP/not_loaded` records, `state/media-readiness.jsonl` evidence, and
  explicit exit-code propagation for 10/20/30/40/50/130.
- `09-media-ready.sh --selftest` covers ready media, media-initializing exit 10,
  ledger output, no move/unload/load invocations, and slot-only skip behavior.
- `10-init-pools.sh` transport fallback now parses `host_status` and only
  treats nonzero values as transport unknown; selftest covers both 0x00 and
  nonzero host status.

## Follow-up Fable Pass

After the coverage fold, Claude Fable 5 was run again through OpenRouter
(`anthropic/claude-fable-5`, routed as
`anthropic/claude-5-fable-20260609`; OpenRouter ID
`gen-1783352129-O30CHOfVDQr0r59eX1mX`).

Verdict: **go for a controlled escalation-disabled physical run, no-go for
unattended/destructive escalation.**

New findings:

- **High:** the readiness poll loop treated any second Unit Attention in a
  load epoch as `RepeatedUnitAttention`. That can falsely terminalize the
  expected LTO-9 sequence `06/29/00 -> 02/04/01 -> 06/28/00 -> GOOD`.
- **Medium:** unclassified non-policy `rem` exit codes in
  `fieldtest/scripts/10-init-pools.sh` could still fall through to the
  escalation ladder if they lacked readiness metadata.
- **Medium:** fieldtest selftest coverage covered the dry-run readiness stop
  path but not unclassified non-policy failures.

Second follow-up fold:

- the readiness epoch now tracks Unit Attention by exact `(ASC, ASCQ)` and
  terminalizes only an identical repeated UA in the same epoch;
- a regression now proves `06/29/00 -> 02/04/01 -> 06/28/00 -> GOOD`
  reaches `Ready`, while identical repeated `06/29/00` remains terminal;
- `10-init-pools.sh` now fails closed on unclassified non-policy exit codes
  before `--force` or `--clobber-data`;
- `10-init-pools.sh --selftest` now covers the unclassified-exit refusal path.

Remaining evidence gap after the system scenario fold: physical MSL3040 capture
when a new LTO-9 cartridge is available.
