# REM S5b Opaque Range Correction Report - 2026-06-16

## Summary

`ReadObjectRange` now matches the sutradhara contract: an empty `file_id` with a
non-zero range is accepted and resolved to the object's sole cataloged payload
member. The requested byte range is interpreted relative to that payload, not
to an inner member file or to the wrapping rem-tar control bytes. A non-empty
`file_id` still uses the S5b member-scoped path.

The daemon range path remains key-free. It streams opaque stored payload bytes
through the existing catalog-backed PFR planner and never decrypts, seals,
looks up a key, or branches on encrypted-vs-plaintext representation.

## What This Proves

- Empty-`file_id` `[start, end)` calls dispatch to `ReadObjectRange` instead of
  failing at the RPC boundary.
- Empty-`file_id` `0,0` still uses the whole-payload `ReadFile` path.
- The catalog-backed path maps payload offset `0` to the first byte of payload
  `S`, not to the wrapping object's manifest.
- Plain payload ranges are byte-exact for mid-slice, slice-to-EOF, empty valid
  range, and whole payload.
- A sealed `RAO1` envelope can be stored as payload `S`, served back by range
  without daemon key material, and decrypted client-side with the test key.
- Past-EOF, arithmetic overflow, and reversed ranges return typed
  `InvalidArgument` errors.

## Coverage Limits

The rem unit tests use `VecBlockSource` and a real catalog projection rather
than a live tape drive. The live VTL consumer check is the system harness
`scenario_rao_archive`; it was flipped to rem-native opaque ranged restore for
both `s-rao-work` and `s-rao-offsite` and passed from a clean slate on
2026-06-16.
