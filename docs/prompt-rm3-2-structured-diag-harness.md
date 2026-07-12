# Prompt RM3.2 — structured read-diag JSON + hermetic measured-backpressure harness

**Status:** pending (gpt-5.6-sol). RM3.2 (after RM3.0 + RM3.1a). Remanence-only. Makes the read-pipeline
backpressure diagnostics machine-readable + adds a hermetic harness that asserts the drive PARKS rather
than shoe-shines. **Physical acceptance is the MSL3040 field window (measured); this milestone delivers
the structured diag + a hermetic simulated-slow-consumer test — NOT the physical run.**
**Normative (read FIRST, binding — do NOT inline):** `docs/design-restore-tape-leg-v0.1.md` **§6.4**
(keep log-scraping — a `GetReadSessionDiag` RPC is NOT wanted; pin the close-line as a STABLE structured
JSON event; emit ONLY at pipeline open/close, never per-block; the harness contracts on the JSON) + §6
context. **The design's line map predates recent changes — SURVEY the current code, cite real lines.**
**Survey + verify against CURRENT code:** `crates/remanence-api/src/read_core.rs` — `ReservoirState`
(~323: `occupancy_bytes`/`park_cycles`/`free_wait_us`/`feed_gap_total_us`/`feed_gap_max_us`/
`feed_gap_samples`), the existing `tracing::info!(target: "remanence_read_diag", …)` lines emitted at
pipeline OPEN and CLOSE (grep `remanence_read_diag`), created per `run_read_pipeline` and dropped on
return. TIO-6 R2's reservoir is already wired here — do NOT touch the hot drive-actor loop.

## Scope
1. **Stable structured diag event.** Upgrade the open/close `remanence_read_diag` tracing lines to a
   STABLE, versioned structured record (e.g. a `#[derive(Serialize)]` struct emitted as a single JSON
   line, or `tracing` structured key=value with a pinned `schema_version` + a fixed key set:
   `park_cycles`, `occupancy_bytes` (or peak/final occupancy), `free_wait_us`, `feed_gap_total_us`,
   `feed_gap_max_us`, `feed_gap_samples`, plus open/close markers + a session correlation id). Emit ONLY
   at pipeline open + close (off the per-block hot path — do NOT add per-iteration diag to the drive
   actor loop; that would be a real throughput risk). Keep it log-scrapable (one JSON line). Document the
   schema (a short reference in the diag module or docs).
2. **Hermetic measured-backpressure harness.** A test that drives a read pipeline with a SIMULATED slow
   consumer (a sink that drains slower than the drive feeds, using the existing test scaffolding / a mock
   drive so it is HERMETIC — no real tape), scrapes the structured close-line JSON, and ASSERTS the
   reservoir PARKED the drive (`park_cycles >= N`) rather than shoe-shining, and that occupancy stayed
   bounded (`occupancy_bytes <= reservoir_high_watermark`). This is the machine-readable acceptance gate;
   the physical MSL3040 leg is the measured confirmation (out of scope here — note it).

## Binding invariants
- Diag stays at OPEN/CLOSE boundaries only — NEVER per-block/per-iteration (hot-path safety). No new RPC
  (log-scraping per §6.4). The read pipeline's behavior (TIO-6 reservoir, throughput) is UNCHANGED — this
  only makes the existing counters machine-readable + adds a test. The structured schema is versioned +
  stable (the harness + future physical acceptance contract on it).

## Tests (verification member — REQUIRED, non-vacuous, no skip)
- The structured diag line is emitted at open + close with the pinned schema (parse it back, assert the
  keys/version).
- The hermetic slow-consumer harness asserts `park_cycles >= N` (drive parked, not shoe-shining) and
  bounded occupancy — a test that would FAIL if the reservoir did not park (e.g. if occupancy were
  unbounded). Non-vacuous.
- No per-block diag added (a quick assertion or a comment that the drive-actor loop is untouched).

## Definition of done (this repo's AGENTS.md)
`cargo build` + `cargo test` + `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` clean
(paste tallies). Summary: files touched (real current lines); the diag schema; the harness assertions;
explicit statement that (a) diag is open/close-only (hot path untouched), (b) no new RPC, (c) the read
pipeline behavior is unchanged, (d) physical MSL3040 acceptance is deferred to the field window. Do NOT
implement ranged AEAD (RM3.3) or the app-restart contract (RM3.1b).
