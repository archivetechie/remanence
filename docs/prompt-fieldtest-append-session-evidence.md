# Codex prompt — fieldtest append-session mode, phase timing, fence accounting, poll ramp

**Repo:** `~/remanence`. **Status:** pending.
**Origin:** `docs/fable-review-msl3040-fieldtest-2026-07-07.md` Track B
(review of the 2026-07-07 MSL3040 physical field session).
**Goal:** make the remaining physical-library time count — separate
append-format behavior from mount-cycle overhead, make every run
self-diagnosing, and stop short readiness settles from costing 30 s each.

**Hard constraints**

- Do NOT touch physical devices, `/dev/sg*`, or the daemon at runtime. All
  verification is hermetic: `--selftest` paths, unit tests, `cargo test`.
- `records.jsonl` `status` stays within the existing vocabulary
  `PASS|FAIL|SKIP|INFO` (design rule in
  `docs/lto9-media-readiness-design-v0.1.md` §12). New signals go in new
  fields, not new statuses.
- Keep existing env-var names (`FIELD_IO_READY_RETRIES`,
  `FIELD_IO_READY_TIMEOUT`, `FIELD_IO_READY_POLL`) working.
- Preserve current script behavior as the default anywhere an operator could
  be mid-runbook; new behavior is opt-in via flags/env except where stated.
- Match the existing style of `fieldtest/scripts/lib.sh` (strict bash,
  evidence-record helpers) and `fieldtest/tools/remfield-io`.

## P1 — `remfield-io write-many` (one session, N appends)

`fieldtest/tools/remfield-io/src/main.rs` currently implements `write` as one
`open_write_session` → one `append_file` → one `close_write_session`
(`main.rs:174-230`). Add a `write-many` subcommand:

- Args: pool, `--count N`, `--size-mib M`, `--caller-object-id-prefix P`
  (each object gets `P-<index>`), plus whatever the existing `write` takes
  (payload generation may reuse the existing deterministic pattern; each
  object's content must differ so SHA-256 fidelity checks are meaningful).
- Opens ONE write session, appends N objects in a loop, closes once.
- Emits one JSON record per object on stdout (JSONL), same shape as `write`
  including `append_commit_info` (tape uuid, tape_file_number, append_mode),
  plus the phase timings from P3.
- Mid-loop failure semantics: if an append fails (poisoned session, sealed
  tape, fence), emit an error record for that object, attempt a clean close,
  and exit non-zero with a distinct exit code so the wrapper can tell
  "session died at object k" from "open failed". Objects 0..k-1 remain
  committed — say so in the final summary record.
- A media-readiness fence error at open must surface the same
  `media-readiness fence operation=<uuid>` text the wrapper already parses
  (`fieldtest/scripts/lib.sh:499-508`).

## P2 — `13-append-loop.sh` dual mode

- Add `--mode cycle|session` (default `cycle` = current behavior, one
  `remfield-io write` per object with full open/close and therefore a
  physical unload/reload per object — this is intentional robot/mount
  stress and stays a supported mode).
- `--mode session` uses `remfield-io write-many` for the write phase; the
  read/fidelity phase is unchanged (per-object reads are fine — reads are a
  different measurement).
- The summary evidence record gains: `mode`, per-object wall-clock latency
  stats (min/median/max seconds), fence count, and total fence-wait seconds
  (from P4 accounting).
- Update `fieldtest/RUNBOOK.md` and `fieldtest/TODAY-MSL3040-GUIDE.md`: the
  recommended physical sequence is now to run the append loop **twice, once
  per mode**, and compare the two summary records; explain in one sentence
  what each mode measures (session = append-format + amortized throughput;
  cycle = full mount-cycle latency + robotics stress).

## P3 — per-object phase timing in evidence

- `remfield-io` (`write`, `write-many`, and the read command) times each RPC
  client-side and includes in its JSON: `open_ms`, `transfer_ms`,
  `close_ms`, `bytes`, and derived `mib_per_s` for the transfer portion.
  For `write-many`, `open_ms`/`close_ms` appear once (on the first/last
  record or the summary record) and each object record carries its own
  `transfer_ms`.
- `fieldtest_capture_io_json` (`fieldtest/scripts/lib.sh:536-566`) records
  per-attempt wall time and, when a fence was hit, the fence wait duration
  (parse `elapsed`/`attempts` from the `wait-ready --json` output it already
  captures) into the evidence record it writes.

## P4 — fence accounting + thresholds in `lib.sh`

- Accumulate per-script-run counters: number of fences encountered, total
  fence-wait seconds, number of I/O calls. At script end (all scripts that
  source `lib.sh` and use `fieldtest_capture_io_json`), emit one
  `INFO` record `fence-summary` with those numbers and the ratio.
- Thresholds, checked per fence wait:
  - `FIELD_READY_WARN_SECS` (default 90): if a single fence wait exceeds
    this, the evidence record gets `readiness_warning=true` and a loud
    stderr line (`[WARN]`-style prefix is fine for stderr; the JSONL status
    stays `INFO`).
  - `FIELD_READY_FAIL_SECS` (default 900): exceeding this logs `FAIL` and
    aborts the retry loop even if wait-ready eventually reported ready.
    Rationale: >15 min on media that is already past first-load optimization
    is neither load settle nor calibration; it is RCA input. Operators doing
    deliberate first-load conditioning use `09-media-ready.sh`, which is not
    subject to these thresholds.
- Document both env vars in the runbook's environment table.

## P5 — unconditional poll ramp in `wait-ready` (small remanence-cli change)

`media_conditioning_poll_interval` (`crates/remanence-cli/src/lib.rs:6704-6711`)
currently applies its 15 s-then-60 s schedule only when the caller's interval
equals `MEDIA_CONDITIONING_STEADY_POLL` exactly; the CLI default `--poll 30s`
(and the fieldtest wrapper) therefore polls at a flat 30 s, so a ~10 s load
settle costs ~30 s.

- Make the ramp unconditional: elapsed < 5 s → 1 s, < 15 s → 2 s, < 60 s →
  5 s, then the configured `--poll` value as the steady-state interval.
  `--poll` keeps its meaning as the steady-state interval; no flag changes.
- Applies to every path that reaches `poll_media_readiness_after_initial_probe`
  (plain `wait-ready`, `--resume`, and the init path — the init path currently
  passes 60 s and must keep an equivalent-or-better early schedule).
- Keep the 250 ms signal slices and the probe-first property (a ready first
  TUR still returns with zero sleep).
- Unit test with the existing fake-clock harness: assert the exact probe
  schedule for a becoming-ready sequence that turns ready at t≈8 s (expect
  ready detected in ≤10 s, not 30 s), and that steady-state respects `--poll`.

## Verification members (required, all hermetic)

- `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test` for
  `remfield-io` and `remanence-cli` (poll-schedule fake-clock test, JSON
  shape tests for `write-many` phase-timing fields).
- Extend `13-append-loop.sh --selftest` (or add one following the pattern of
  `09-media-ready.sh --selftest` / `10-init-pools.sh --selftest`) covering:
  mode flag parsing (`cycle` default, `session` accepted, junk rejected),
  fence-summary record emission, and threshold classification
  (warn field set above `FIELD_READY_WARN_SECS`, FAIL above
  `FIELD_READY_FAIL_SECS`) using stubbed wait-ready JSON.
- Run every existing `fieldtest/scripts/*.sh --selftest` and report results.
- Report at the end: files touched, test/gate summary, and any deviation from
  this prompt with rationale.
