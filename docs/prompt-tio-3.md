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
