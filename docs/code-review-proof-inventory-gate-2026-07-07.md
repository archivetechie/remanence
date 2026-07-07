# Code review -- proof inventory gate

Review target: commit `01d1c08` (`verif: add proof inventory gate`).

Reviewer: GLM 5.2 via OpenRouter (`z-ai/glm-5.2`, routed as
`z-ai/glm-5.2-20260616`) plus Codex verification.

OpenRouter ID: `gen-1783393765-icg8aeUZ0txyngKzKDAp`.

## Verdict

GLM found two actionable Medium issues in the new gate. Both were accepted and
folded in the follow-up hardening commit.

No Critical or High findings.

## Accepted Findings

- **Medium:** the original `fn drift_guard` text check could be fooled by a
  comment, and `cargo test drift_guard` can return success even when no test is
  selected. Fold: `make proof-inventory` now runs
  `cargo test drift_guard -- --list` and requires an actual
  `drift_guard: test` entry before executing the filtered test.
- **Medium:** the original placeholder scan applied Lean placeholder words to
  Rust sources, which could false-red on ordinary English comments. Fold: the
  placeholder scan now targets maintained Lean proof files only, excluding
  build caches and proof-search scratch transcripts.
- **Low:** the script relies on GNU `find`/`sort` behavior. Fold: the script
  now states the Linux/GNU tooling assumption explicitly, matching the repo's
  development target.
- **Low:** the docs implied `lake build` was universal without saying that this
  is an enforced current-proof-crate invariant. Fold: `verif/STATUS.md` now
  says every current proof crate must have `lean/lakefile.toml`.

## Review Notes

The remaining GLM notes were non-blocking:

- Running `cargo test drift_guard` and then full `cargo test` repeats the drift
  guard. This is intentional: the first run gives focused attribution, while
  the full test run preserves the crate's normal local gate.
- Checking for `SPEC.md` in every proof crate would be reasonable future
  polish, but the reviewed commit's stated gate was drift guards, Rust tests,
  Lean builds, and placeholder scanning.

After the fold, the gate is suitable as the local proof-inventory replay check.
