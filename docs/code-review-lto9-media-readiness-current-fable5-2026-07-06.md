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
