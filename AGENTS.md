# Agent conventions (codex and others)

Read CLAUDE.md. Non-negotiables:
1. **Run what you changed**: `cargo test`, `cargo fmt --check`,
   `cargo clippy -- -D warnings` — paste outputs. CI enforces the same.
2. **`cargo build --release` before finishing** — the ~/system harness runs
   the release binaries and its freshness guard fails on stale ones.
3. **Commit; never leave the tree dirty** (WIP → `wip/<topic>`).
4. Facts that bite: never assume one library (mainlib + d2lib coexist);
   proto changes regenerate sutradhara's `_proto` (coordinate); tape identity
   is library-independent (BOT uuid/barcode).
