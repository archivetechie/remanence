# Agent conventions (codex and others)

Read CLAUDE.md. Non-negotiables:
1. **Run what you changed**: `cargo test`, `cargo fmt --check`,
   `cargo clippy -- -D warnings` — paste outputs. CI enforces the same.
2. **A test never silently passes.** If a test can't run its subject —
   missing tool (`bsdtar`/`attr`), absent fixture, unavailable device — it must
   **`assert!`/`panic!`** (or be `#[ignore]`d with a reason), never
   eprintln-and-`return` to a green result. A skipped-as-passed test reads as
   coverage and proves nothing; that gap shipped the §A3.5 fidelity test
   vacuously-green and the H3 finding. Pin the dependency (CI installs it) or
   fail loudly.
3. **`cargo build --release` before finishing** — the ~/system harness runs
   the release binaries and its freshness guard fails on stale ones.
4. **Commit; never leave the tree dirty** (WIP → `wip/<topic>`).
5. Facts that bite: never assume one library (mainlib + d2lib coexist);
   proto changes regenerate sutradhara's `_proto` (coordinate); tape identity
   is library-independent (BOT uuid/barcode).
