# remanence — working conventions

## What this repo is
The Rust tape stack (layers 1–5): SCSI drive/changer control, on-tape format
(+RS parity), tape catalog/state, Layer-5 gRPC daemon (`rem-daemon`: catalog,
write/read sessions, library control, mTLS) and CLI (`rem-debug`). Crates
under `crates/`. State at /var/lib/replica/rem (config.toml, rem-state.sqlite).

## Verify
`cargo test` + `cargo fmt --check` + `cargo clippy -- -D warnings` (CI gates
all three). End-to-end: `~/system` harness scenarios (D–N) over the daemon.

## The traps that bite
- **The harness runs `target/release/{rem-debug,rem-daemon}` — REBUILD after
  changes** (`cargo build --release`); the harness freshness guard flags stale
  binaries, don't make it.
- Two VTLs in QuadStor: mainlib (rem, changer rev D.00) + d2lib (d2tape,
  D2D0). Never assume a single library; scope by configured serial.
- SCSI needs the `tape` group (`sg tape -c …`) and cap_sys_rawio on rem-debug.
- Known open issue: tape-recycle catalog↔BOT-uuid skew
  (`docs/tape-recycle-identity-reconciliation-concern.md`) — retire/rebind
  machinery landed, recycle-script integration pending.

## Pattern + hygiene
Design/prompt docs in `docs/`; codex or Claude implements; harness verifies.
`gardener` auto-commits/pushes/prunes in the background (a worktree holds
branch 2a-3-cli-on-new-design — protected). Docs lifecycle: `docs/INDEX.md`.
