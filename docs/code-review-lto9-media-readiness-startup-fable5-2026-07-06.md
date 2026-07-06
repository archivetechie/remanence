# Code review -- LTO-9 media-readiness startup reconciliation, Fable 5

**Date:** 2026-07-06  
**Reviewer:** Claude Fable 5 via OpenRouter
(`anthropic/claude-fable-5`, routed as `anthropic/claude-5-fable-20260609`)  
**Scope:** uncommitted startup-reconciliation slice for
`docs/lto9-media-readiness-design-v0.1.md`: daemon bringup reconciliation and
non-dry-run direct `rem tape init` reconciliation of active SQLite
`media_readiness_ops` fences.

## Verdict

Initial review was conditionally acceptable, with one blocker and one high
safety finding accepted and folded. The verify pass approved the revised diff
for merge after those fixes.

## Initial findings and disposition

- **Blocker, accepted:** startup probing allowed a record with no barcode to
  reach `TEST UNIT READY`. This violated the design requirement that missing
  serial/barcode binding remains fenced. Fold: barcodeless or blank-barcode
  records now record `aborted_unknown` with selected-library snapshot scope and
  do not construct a probe plan.
- **High, accepted:** startup TUR `GOOD` could clear a prior
  `timeout_unknown` or `transport_unknown` completion-unknown fence. Fold:
  release-required prior states now record
  `startup_reconcile_requires_release`, preserve the state, dirty scope,
  evidence, and quarantine id, and never issue startup TUR.
- **High, verified as covered by admission:** `unit_attention` does not get a
  quarantine id, but it remains active with `drive+tape` dirty scope. Admission
  checks fence active rows whose dirty scope is not `none`.
- **Medium, residual:** an unparsable `operation_id` still fails closed for the
  whole reconciliation loop. This can block startup, but it is safer than
  attempting to modify a row that cannot be addressed.
- **Medium, residual:** direct CLI reconciliation has a discovery-to-init
  time-of-check/time-of-use window. This remains under the larger
  daemon-resource-owner/direct-CLI gate.
- **Medium, verified externally:** drive serial trust depends on discovery and
  `open_drive` identity behavior outside this slice. The startup plan still
  refuses to probe unless selected-library drive serial and barcode bindings
  match the discovery snapshot.

## Verify pass

Fable 5 verified the revised diff and reported both accepted findings resolved:

- missing barcode now returns `KeepFenced` before any probe can be built;
- release-required prior states are checked before drive/serial/barcode
  resolution and cannot be cleared by startup TUR.

The verify pass raised one minor audit-continuity issue: a release-required
transition initially overwrote an existing quarantine id with the derived
`mrq-<operation_id>`. That was folded after the verify pass; existing
quarantine ids are now preserved, with the derived id used only as a fallback.

## Residual risk

This review does not close the remaining design gates: signal/interrupt
reconciliation, READ ELEMENT STATUS secondary evidence, scenario/chaos
coverage, and the broader daemon-owned direct-hardware resource model.

