# Code review -- LTO-9 media-readiness RES evidence, Fable 5

**Date:** 2026-07-06
**Reviewer:** Claude Fable 5 via OpenRouter
(`anthropic/claude-fable-5`, routed as `anthropic/claude-5-fable-20260609`)
**OpenRouter IDs:** `gen-1783348997-DNcvHG5erfiYx8RGOt1U`,
`gen-1783349090-3Cj4VIBx9VR2CrVsSd41`
**Scope:** uncommitted READ ELEMENT STATUS secondary-evidence slice for
`docs/lto9-media-readiness-design-v0.1.md` §14.

## Verdict

Initial review found two high findings and three medium/low findings. The
accepted findings were folded into this slice before commit.

## Findings and disposition

- **High, accepted:** new `ElementException` behavior was untested. Fold:
  added model tests for drive/slot/IE projection, EXCEPT-clear stale ASC/ASCQ
  dropping, and EXCEPT-set `00/00`; added CLI JSON and human rendering tests.
- **High, accepted:** the design says daemon proto/API fields are out of scope
  until physical evidence justifies them. Fold: verified model structs are not
  serialized directly and added an API projection regression test that starts
  with exception evidence but asserts the projected proto state has no
  exception-shaped field.
- **Medium, constrained:** CLI JSON emits `exception: null` for healthy
  elements rather than omitting the key. Disposition: kept the explicit-null
  shape to match the existing stable CLI JSON style, and pinned it in tests.
- **Medium, accepted/documented:** the model intentionally drops descriptor
  ASC/ASCQ when EXCEPT is clear even though the low-level parser retains exact
  bytes. Fold: added a code comment and a test locking the boundary.
- **Low, accepted:** human output used bare hex while JSON used `0x` prefixes.
  Fold: human output now prints `exception ASC/ASCQ=0x04/0x01`.

## Residual risk

This closes the READ ELEMENT STATUS secondary-evidence implementation slice.
The larger LTO-9 media-readiness effort still requires scenario/chaos coverage
for RDY-01 and destructive-escalation refusal before the design can be marked
fully implemented.
