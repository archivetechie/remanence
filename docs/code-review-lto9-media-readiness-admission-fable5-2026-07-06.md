# Code review -- LTO-9 media-readiness admission, Fable 5

**Date:** 2026-07-06  
**Reviewer:** Claude Fable 5 via OpenRouter
(`anthropic/claude-fable-5`, routed as `anthropic/claude-5-fable-20260609`)  
**Scope:** uncommitted admission-control slice for
`docs/lto9-media-readiness-design-v0.1.md`: SQLite-backed media-readiness
fence checks for `rem tape init`, daemon session opens, daemon robotics, and
session close/unmount paths.

## Verdict

No accepted blocker remains after disposition. The review found one false
positive blocker and several valid safety findings that were folded into the
implementation.

## Findings and disposition

- **B1, false positive:** Fable flagged `mount.bay` as a possible logical bay
  index mismatch. Local code review showed `resolve_load_target()` returns
  `DriveBay.element_address`, and mount tests use element addresses such as
  `0x0101`; no code change needed.
- **H1, accepted:** Admission checks were plain read-before-act. Daemon session
  admission now rechecks after drive reservation, and daemon robotics now
  reserves the drive pool before checking SQLite admission. The direct CLI
  `tape init` path still has no daemon hardware lock; this remains a known
  residual until CLI direct hardware access is brought under the same resource
  owner.
- **H2, accepted:** A session that starts with an already-loaded tape can still
  use the robot on close if a home slot is known. Mounted sessions now remember
  library serial and barcode, session opens treat `home_slot` as robotics use,
  and close/abort paths recheck admission before unload/move-home.
- **M1, accepted:** `dirty_scope.contains("library")` was too loose. Scope
  matching now tokenizes exact scope words, treats `drive+tape` as local, and
  fails closed for unknown scope tokens.
- **M2, accepted:** Barcode matching was asymmetric. Admission now trims and
  compares recorded/caller barcodes case-insensitively.
- **M3, accepted via H1:** Robotics dispatch now holds the exclusive drive-pool
  reservation before the SQLite admission decision and releases it on any
  admission/index failure.

## Residual risk

The slice is safer but not the full MR design. Remaining gates are startup
reconciliation, signal/interrupt reconciliation, READ ELEMENT STATUS secondary
evidence, scenario/chaos coverage, and a daemon-owned resource model for direct
CLI hardware operations.

## Verify pass

A second Fable 5 verify pass was run on the revised diff.

- **Accepted:** Close paths now check admission even when `home_slot` is
  absent; they pass `library_robotics=false` but still enforce same-drive and
  same-barcode fences.
- **Accepted:** Missing or empty `dirty_scope` on an active row now fails
  closed as a whole-library blocker.
- **Residual:** There remains a check-then-act window between SQLite admission
  reads and hardware I/O, especially for direct CLI hardware operations. This
  is documented as a later daemon-resource-owner gate rather than solved by
  this slice.
- **Not accepted as current bug:** The barcodeless-media warning does not map
  to a normal admission path because session resolution is voltag/catalog
  driven. If Remanence later supports unbarcoded physical media, admission must
  add a stronger library-level guard for that path.
