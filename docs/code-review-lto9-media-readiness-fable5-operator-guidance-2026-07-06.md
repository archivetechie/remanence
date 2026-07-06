# Claude Fable 5 review: LTO-9 readiness operator guidance

**Date:** 2026-07-06  
**Reviewer:** Claude Fable 5 via OpenRouter  
**Model route:** `anthropic/claude-fable-5` -> `anthropic/claude-5-fable-20260609`  
**Usable generation:** `gen-1783354536-1mpbGKgqTQjJB8r6GLr8`  
**Prompt scope:** `rem tape wait-ready`, `fieldtest/scripts/09-media-ready.sh`,
`fieldtest/scripts/10-init-pools.sh`, and the `~/system` LTO9 readiness scenario.

Operational note: initial calls to the Fable route returned `content=null`
because the completion budget was consumed by reasoning tokens. The usable
call set `reasoning.max_tokens=2048`, `reasoning.exclude=true`, and
`max_tokens=8192`.

## Verdict

Fable returned **NO-GO** for the physical MSL3040 run until the operator-safety
and escalation findings were folded.

## Accepted Findings

1. **Blocker: `wait-ready --json` lacked `operator_action` and
   `recommended_next_command`.**  
   Fold: add both fields to normal and failure JSON output.

2. **Blocker: human `wait-ready` output lacked stderr recovery guidance for
   non-ready exits.**  
   Fold: print `operator_action` and `recommended_next_command` to stderr for
   non-ready human output and ownership/refusal failures.

3. **Major: `09-media-ready.sh` mislabeled exit 50 as only
   `reservation_conflict`.**  
   Fold: use `ownership_refused` for fallback state and evidence text, because
   exit 50 also covers wrong library, absent barcode, ambiguous barcode, and
   allowlist refusal.

4. **Major: `10-init-pools.sh` could escalate generic exit 1 by absence of
   readiness evidence.**  
   Fold: fail closed unless plain init output explicitly contains
   `decision: require-force:` before `--force`, and force output explicitly
   contains `decision: refuse-clobber:` before `--clobber-data`.

5. **Major: per-level init evidence could be overwritten during escalation.**  
   Fold: write separate dry-run/plain/force/clobber artifacts.

6. **Coverage gap: system scenario did not run `09-media-ready.sh --selftest`.**  
   Fold: add a scenario step and `covers` entry for the media-ready wrapper.

## Remaining Physical Gate

The design still requires a controlled physical MSL3040 capture on a known-new
LTO-9 cartridge: `rem tape wait-ready --json`, selected-library slots
before/after, dmesg SCSI/HBA window, and fieldtest evidence showing no D2/LTO-7
state-changing commands.
