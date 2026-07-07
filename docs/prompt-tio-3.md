# Codex prompt — TIO-3: staged overlap (double-buffered transfer)

**Repo:** `~/remanence`. **Status:** pending (depends on TIO-2 landed).
**Normative design:** `docs/design-tape-io-throughput-v0.1.md` (FROZEN v0.2);
§5 (L3), §5.1 (crash table), §9 (tests).

Deliverables (design §10 TIO-3):

1. Write path: producer thread reads spool-file batches; drive actor drains
   into batched WRITEs; bounded channel depth 2; deterministic
   drain-and-poison on sink error; commit ordering unchanged (filemark only
   after every data batch returned clean).
2. Read path: drive actor produces batches; gRPC sender consumes.
3. Crash-table conformance (§5.1): each row's recovery rule holds.

Verification member (hermetic, required): chaos/kill coverage asserting every
§5.1 row; source-error and sink-error mid-stream drain deterministically with
no early journal/ack; ordering assertion that no filemark CDB precedes the
final clean batch. fmt/clippy/`cargo test` for touched crates. Report files
touched, gates, deviations.

---
**Diff gate 2026-07-07 (Claude Fable 5): PASS.** Independent gates: fmt/clippy
clean, full `cargo test` green (42 suites incl. the daemon socket tests
sandbox-blocked for codex). Verified: staged sink routes filemarks through
the ordered command channel behind flush_pending — ordering by construction
plus the explicit l3_ordering test; crash rows covered by
l3_crash_after_producer_read / l3_source_error_mid_stream /
l3_sink_error_with_queued_buffers + read-sender failure test. Disposition:
the §5.1 tripwire-mismatch and filemark-before-journal rows are owned by
TIO-2's drift-fence tests and layer-5's journal-boundary machinery
respectively — no L3-specific state exists in those windows (buffers are
process-local and unjournaled), so no additional coverage required.
